//! EMI (equivalence modulo inputs) harness for the optimizer: [`calibrate_corpus`]
//! keeps the sources an injected `builtin::black_box(false)` guard leaves
//! alone, and [`mutate_corpus`] reports the ones a [`Payload`] behind it moves.
//!
//! The material comes from every [`Root`]: the e2e fixtures, the stdlib modules
//! that carry `test` blocks, and the `example/` programs.
//!
//! The design and what is left to build are in
//! [WEP: Compiler Fuzzing](../../docs/wep-2026-08-19-compiler-fuzzing.md).
//!
//! ```sh
//! cargo test --test emi -- --ignored --nocapture
//! ```
//!
//! Knobs: `WADO_EMI_JOBS`, `WADO_EMI_FILTER`, `WADO_EMI_ROOTS`,
//! `WADO_EMI_SHARD` (`k/n`), `WADO_EMI_LIMIT`, `WADO_EMI_OUT`.

mod common;

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use wado_compiler::ast::{
    AstVisitor, Block, Condition, ConditionElement, Expr, Function, Item, MatchExpr, Module, Param,
    Pattern, SelfKind, Stmt, walk_block, walk_expr, walk_function, walk_item, walk_stmt,
};
use wado_compiler::hashmap::{IndexMap, IndexSet};
use wado_compiler::{CompilerOptions, OptLevel};

/// Levels the calibration compares. `O0` is the reference the optimizer must
/// agree with; `O3` runs every pass the most times, so it is where a guard is
/// most likely to perturb something.
const CALIBRATION_LEVELS: [OptLevel; 2] = [OptLevel::O0, OptLevel::O3];

// ---------------------------------------------------------------------------
// Guard
// ---------------------------------------------------------------------------

/// Wrap `payload` in a guard the compiler cannot decide.
///
/// Single-line by construction: an injection must not move the code after it,
/// or every source that reports a line of its own would drop out of the corpus.
fn guard(payload: &str) -> String {
    format!("if builtin::black_box(false) {{ {payload} }} ")
}

// ---------------------------------------------------------------------------
// Corpus
// ---------------------------------------------------------------------------

/// Where the campaign draws its material from.
///
/// A root supplies what a `.wado` file does not state about itself: which files
/// under it are programs, and how one is run.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Root {
    /// The e2e fixtures — one file, one program, `__DATA__` carrying the spec.
    Fixtures,
    /// The standard library, whose `test` blocks are the observable output.
    Stdlib,
    /// The examples: CLI programs observed through stdout. Only the top-level
    /// files; the directories under it are packages, which the runner cannot
    /// draw from yet.
    Example,
}

const ROOTS: [Root; 3] = [Root::Fixtures, Root::Stdlib, Root::Example];

impl Root {
    fn name(self) -> &'static str {
        match self {
            Root::Fixtures => "fixtures",
            Root::Stdlib => "stdlib",
            Root::Example => "example",
        }
    }

    /// The root's directory, relative to the repository root — which is also
    /// the prefix every name under it carries.
    fn rel_dir(self) -> &'static str {
        match self {
            Root::Fixtures => "wado-compiler/tests/fixtures",
            Root::Stdlib => "wado-compiler/lib",
            Root::Example => "example",
        }
    }

    fn dir(self) -> PathBuf {
        repo_root().join(self.rel_dir())
    }

    /// The stdlib is a tree; the other two are flat.
    fn is_recursive(self) -> bool {
        self == Root::Stdlib
    }

    /// How a source here runs when it carries no `__DATA__` section.
    ///
    /// An example is a `wasi:cli/command` program — the world it declares
    /// `export fn run()` for. Everything else is run under the test world, the
    /// e2e harness's default.
    fn default_spec(self) -> Spec {
        Spec {
            test_world: self != Root::Example,
            allocator: "debug".to_string(),
        }
    }

    /// Can an injection into this file be observed at all? Only what the world
    /// enters can move an output, and a library it cannot enter is not material.
    ///
    /// A fixture is a program by construction. A stdlib module or an example
    /// with neither a `test` block nor `export fn run` would fail its baseline
    /// compile, landing in the report where a regression belongs.
    fn is_subject(self, path: &Path) -> bool {
        if self == Root::Fixtures {
            return true;
        }
        let source = std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
        let Ok(spec) = Spec::parse(self, &source) else {
            // A `__DATA__` key the harness does not understand is an exclusion
            // to report, not material to hide.
            return true;
        };
        let items = wado_compiler::parse(&source).ast.items;
        items.iter().any(|item| {
            if spec.test_world {
                matches!(item, Item::Test(_))
            } else {
                matches!(item, Item::Function(f) if f.is_export && f.name == "run")
            }
        })
    }
}

/// One program in the corpus.
struct Source {
    path: PathBuf,
    root: Root,
}

impl Source {
    /// The corpus identity: the path relative to the repository root. A bare
    /// file name would not do — `json_test.wado` names two programs.
    fn name(&self) -> String {
        self.path
            .strip_prefix(repo_root())
            .expect("a corpus path is under the repository root")
            .to_string_lossy()
            .to_string()
    }

    /// Recover a source from the name [`Source::name`] wrote.
    fn from_name(name: &str) -> Self {
        let root = ROOTS
            .into_iter()
            .find(|root| name.starts_with(root.rel_dir()))
            .unwrap_or_else(|| panic!("`{name}` is under no corpus root"));
        Self {
            path: repo_root().join(name),
            root,
        }
    }
}

/// Is this `__DATA__` key one the harness understands?
///
/// `test` and `allocator` select the world and the allocator. Every other
/// understood key states an expectation, which the baseline run supersedes —
/// EMI compares a mutant against the program it came from, not against the
/// source's recorded output. A key that is *not* understood either feeds the
/// runner an input the comparison does not reproduce (a request, a preopen,
/// stdin, a compile-time parameter) or was added after this list was written;
/// either way the source leaves the corpus instead of being run with the input
/// silently missing.
fn key_is_understood(key: &str) -> bool {
    matches!(
        key,
        "test"
            | "allocator"
            | "stdout"
            | "stdout_contains"
            | "stderr"
            | "stderr_contains"
            | "trapped"
            | "exit_code"
            | "skip_os"
            | "warnings_contains"
            | "warnings_not_contains"
    ) || key.starts_with("wir_expect:")
        || key.starts_with("wir_not_expect:")
}

/// How a source must be compiled and run.
struct Spec {
    test_world: bool,
    allocator: String,
}

impl Spec {
    /// A source with no `__DATA__` section runs the way its root says.
    fn parse(root: Root, source: &str) -> Result<Self, Excluded> {
        let Some(data) = common::extract_data_section(source) else {
            return Ok(root.default_spec());
        };
        let value: serde_json::Value =
            serde_json::from_str(data).map_err(|e| Excluded::MalformedData(e.to_string()))?;
        let object = value
            .as_object()
            .ok_or_else(|| Excluded::MalformedData("__DATA__ is not an object".to_string()))?;
        for key in object.keys() {
            if !key_is_understood(key) {
                return Err(Excluded::UnsupportedDataKey(key.clone()));
            }
        }
        Ok(Self {
            test_world: object.contains_key("test"),
            allocator: object
                .get("allocator")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("debug")
                .to_string(),
        })
    }

    fn compiler_options(&self, opt_level: OptLevel) -> CompilerOptions {
        CompilerOptions {
            opt_level,
            target_world: self.test_world.then(|| "test".to_string()),
            allocator: Some(self.allocator.clone()),
            ..Default::default()
        }
    }
}

/// Why a source cannot serve as an EMI oracle.
#[derive(Debug)]
enum Excluded {
    MalformedData(String),
    UnsupportedDataKey(String),
    Todo,
    FormatFailed(String),
    NoInjectionSite,
    /// Nothing in scope at any site, so no payload has anything to name.
    NoBindingInScope,
    BaselineCompileFailed {
        level: OptLevel,
        detail: String,
    },
    BaselineUnhealthy {
        level: OptLevel,
        detail: String,
    },
    GuardRejected {
        level: OptLevel,
        detail: String,
    },
    /// The empty guard moved the program's output — the source observes
    /// something an injection perturbs, so a real mutation could not be told
    /// apart from that.
    GuardChangedOutput {
        level: OptLevel,
        detail: String,
    },
    /// The source's own output moves between runs, so no mutant can be
    /// compared against it.
    Nondeterministic {
        level: OptLevel,
        detail: String,
    },
    /// Not an exclusion but a finding: an unreachable guard crashed the
    /// compiler or the runtime. The campaign reports these separately and
    /// fails on them.
    GuardCrashed {
        level: OptLevel,
        detail: String,
    },
}

impl Excluded {
    /// Bucket name for the grouped report.
    fn kind(&self) -> &'static str {
        match self {
            Excluded::MalformedData(_) => "malformed __DATA__",
            Excluded::UnsupportedDataKey(_) => "unsupported __DATA__ key",
            Excluded::Todo => "TODO module",
            Excluded::FormatFailed(_) => "formatter rejected the source",
            Excluded::NoInjectionSite => "no injection site",
            Excluded::NoBindingInScope => "no binding in scope",
            Excluded::BaselineCompileFailed { .. } => "baseline failed to compile",
            Excluded::BaselineUnhealthy { .. } => "baseline did not pass",
            Excluded::GuardRejected { .. } => "guard failed to compile",
            Excluded::GuardChangedOutput { .. } => "guard changed the output",
            Excluded::Nondeterministic { .. } => "source is nondeterministic",
            Excluded::GuardCrashed { .. } => "guard crashed the compiler",
        }
    }

    fn detail(&self) -> String {
        match self {
            Excluded::MalformedData(d) | Excluded::FormatFailed(d) => d.clone(),
            Excluded::UnsupportedDataKey(k) => k.clone(),
            Excluded::Todo | Excluded::NoInjectionSite | Excluded::NoBindingInScope => {
                String::new()
            }
            Excluded::BaselineCompileFailed { level, detail }
            | Excluded::BaselineUnhealthy { level, detail }
            | Excluded::GuardRejected { level, detail }
            | Excluded::GuardChangedOutput { level, detail }
            | Excluded::Nondeterministic { level, detail }
            | Excluded::GuardCrashed { level, detail } => {
                format!("[{}] {detail}", common::opt_level_name(*level))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Injection sites
// ---------------------------------------------------------------------------

/// A position a guard may be inserted at, and what stands there.
#[derive(Clone)]
struct Site {
    offset: usize,
    kind: &'static str,
    /// The bindings in scope here a payload may write to.
    mutables: Vec<String>,
    /// The bindings in scope here, all of them. A payload may read any.
    readables: Vec<String>,
}

/// A binding a payload may name, and whether it may be assigned to.
#[derive(Clone)]
struct Binding {
    name: String,
    is_mut: bool,
}

fn stmt_kind(stmt: &Stmt) -> &'static str {
    match stmt {
        Stmt::Let(_) => "let",
        Stmt::Expr(_) => "expr",
        Stmt::Return(_) => "return",
        Stmt::TaskReturn(_) => "task-return",
        Stmt::If(_) => "if",
        Stmt::While(_) => "while",
        Stmt::For(_) => "for",
        Stmt::ForOf(_) => "for-of",
        Stmt::Loop(_) => "loop",
        Stmt::Match(_) => "match",
        Stmt::Break(_) => "break",
        Stmt::Continue(_) => "continue",
        Stmt::Assert(_) => "assert",
        Stmt::LabeledBlock(_) => "labeled-block",
        Stmt::Item(_) => "item",
        Stmt::Error(_) => "error",
    }
}

/// Collects the positions at which a guard may be inserted.
///
/// A site is the start of a statement inside a function or `test` body. Only
/// positions *before* a statement qualify: the end of a block may be the
/// block's value, and a guard there would change it. Bodies are the only place
/// collected — a guard in a global initializer or another compile-time context
/// stops it being constant, which is a compile error rather than a mutant.
struct SiteCollector {
    sites: Vec<Site>,
    body_depth: u32,
    /// Bindings per open block, innermost last.
    scopes: Vec<Vec<Binding>>,
    /// Where the innermost body's own scopes start. Below it stands a closure
    /// capture: readable, but not assignable.
    capture_floor: usize,
}

impl SiteCollector {
    fn collect(module: &Module) -> Vec<Site> {
        let mut collector = Self {
            sites: Vec::new(),
            body_depth: 0,
            scopes: Vec::new(),
            capture_floor: 0,
        };
        for item in &module.items {
            collector.visit_item(item);
        }
        collector.sites.sort_by_key(|site| site.offset);
        collector.sites.dedup_by_key(|site| site.offset);
        collector.sites
    }

    /// The bindings in scope, a shadowed name answered for by the one that
    /// shadows it: an outer `let mut` an inner `let` shadows is not assignable
    /// through that name.
    fn visible(&self) -> IndexMap<&str, (usize, bool)> {
        let mut visible = IndexMap::default();
        for (depth, scope) in self.scopes.iter().enumerate() {
            for binding in scope {
                visible.insert(binding.name.as_str(), (depth, binding.is_mut));
            }
        }
        visible
    }

    fn visible_mutables(&self) -> Vec<String> {
        self.visible()
            .iter()
            .filter(|(_, (depth, is_mut))| *is_mut && *depth >= self.capture_floor)
            .map(|(name, _)| (*name).to_string())
            .collect()
    }

    fn visible_readables(&self) -> Vec<String> {
        self.visible()
            .keys()
            .map(|name| (*name).to_string())
            .collect()
    }

    /// Enter a function or `test` body with its own scope stack, seeded with
    /// its parameters, so a nested body does not inherit bindings it cannot
    /// name.
    fn in_body(&mut self, params: Vec<Binding>, walk: impl FnOnce(&mut Self)) {
        self.body_depth += 1;
        let outer = std::mem::take(&mut self.scopes);
        let floor = std::mem::replace(&mut self.capture_floor, 0);
        self.scopes.push(params);
        walk(self);
        self.scopes = outer;
        self.capture_floor = floor;
        self.body_depth -= 1;
    }

    /// Enter a closure body, which keeps the enclosing scopes: a capture is a
    /// name the body may read. The floor rises so the write payload does not
    /// reach one.
    fn in_closure(&mut self, params: Vec<Binding>, walk: impl FnOnce(&mut Self)) {
        self.body_depth += 1;
        let floor = std::mem::replace(&mut self.capture_floor, self.scopes.len());
        self.scopes.push(params);
        walk(self);
        self.scopes.pop();
        self.capture_floor = floor;
        self.body_depth -= 1;
    }

    /// Walk what a binding construct — a loop, a `match` arm, an `if let` —
    /// scopes its bindings over.
    fn in_scope(&mut self, bindings: Vec<Binding>, walk: impl FnOnce(&mut Self)) {
        self.scopes.push(bindings);
        walk(self);
        self.scopes.pop();
    }
}

/// The bindings a statement introduces.
///
/// An uninitialized `let mut x: i32;` is left out: a payload that reads it
/// before its first assignment would not compile.
fn let_bindings(stmt: &Stmt) -> Vec<Binding> {
    let Stmt::Let(let_stmt) = stmt else {
        return Vec::new();
    };
    if let_stmt.is_reactive || let_stmt.value.is_none() {
        return Vec::new();
    }
    // `let mut x` carries the `mut` on the statement; `MutIdent` is what a
    // nested pattern binds.
    let mut bindings = Vec::new();
    let refutable = let_stmt.else_block.is_some();
    pattern_bindings(&let_stmt.pattern, let_stmt.is_mut, refutable, &mut bindings);
    bindings
}

/// A plain name in a refutable pattern is either a binding or a unit variant
/// and the parser cannot tell, so an initial capital is read as the variant
/// Wado spells that way: naming one costs the payload the whole fixture.
fn pattern_bindings(pattern: &Pattern, is_mut: bool, refutable: bool, out: &mut Vec<Binding>) {
    let recurse = |pattern, out: &mut Vec<Binding>| {
        pattern_bindings(pattern, is_mut, refutable, out);
    };
    match pattern {
        Pattern::Ident { name, .. } => {
            if !refutable || !name.starts_with(char::is_uppercase) {
                out.push(Binding {
                    name: name.clone(),
                    is_mut,
                });
            }
        }
        Pattern::MutIdent { name, .. } => out.push(Binding {
            name: name.clone(),
            is_mut: true,
        }),
        Pattern::Tuple(patterns, _) => {
            for pattern in patterns {
                recurse(pattern, out);
            }
        }
        Pattern::Variant { bindings, .. } => {
            for pattern in bindings {
                recurse(pattern, out);
            }
        }
        Pattern::Struct { fields, .. } => {
            for field in fields {
                recurse(&field.pattern, out);
            }
        }
        // An `|` alternative binds the same names in each branch, so the first
        // answers for all of them; the rest bind nothing new.
        Pattern::Or(patterns) => {
            if let Some(first) = patterns.first() {
                recurse(first, out);
            }
        }
        Pattern::Literal(_) | Pattern::Wildcard | Pattern::Range { .. } | Pattern::Error(_) => {}
    }
}

/// What a `let` chain binds over the body it guards. A `mut` leaf carries its
/// own mutability; the chain itself declares none.
fn condition_bindings(condition: &Condition) -> Vec<Binding> {
    let Condition::LetChain { elements, .. } = condition else {
        return Vec::new();
    };
    let mut bindings = Vec::new();
    for element in elements {
        if let ConditionElement::Let { pattern, .. } = element {
            pattern_bindings(pattern, false, true, &mut bindings);
        }
    }
    bindings
}

fn params_of(params: &[Param]) -> Vec<Binding> {
    params
        .iter()
        // A `self` receiver is not a name a payload may write to, and reading
        // it is what `self.field` already does.
        .filter(|param| param.self_kind == SelfKind::None)
        .map(|param| Binding {
            name: param.name.clone(),
            is_mut: param.is_mut,
        })
        .collect()
}

impl AstVisitor for SiteCollector {
    fn visit_item(&mut self, item: &Item) {
        // A `test` body is a function body in every way that matters here; the
        // rest reach their bodies through `visit_function`.
        if matches!(item, Item::Test(_)) {
            self.in_body(Vec::new(), |s| walk_item(s, item));
        } else {
            walk_item(self, item);
        }
    }

    fn visit_function(&mut self, func: &Function) {
        self.in_body(params_of(&func.params), |s| walk_function(s, func));
    }

    /// A closure body keeps the enclosing scopes: it may read a capture, while
    /// assigning to one would promote it to `fn mut` and every call through a
    /// plain `let` would stop compiling.
    fn visit_expr(&mut self, expr: &Expr) {
        let Expr::Closure(closure) = expr else {
            walk_expr(self, expr);
            return;
        };
        let params = closure
            .params
            .iter()
            .map(|param| Binding {
                name: param.name.clone(),
                is_mut: param.is_mut,
            })
            .collect();
        self.in_closure(params, |s| walk_expr(s, expr));
    }

    /// The statements that scope a binding over a body. An `else if` is an
    /// `else` block holding one `If`, and a guard injected in front of it would
    /// split the chain, so that `If` is visited directly instead.
    fn visit_stmt(&mut self, stmt: &Stmt) {
        match stmt {
            Stmt::If(if_stmt) => {
                self.visit_condition(&if_stmt.condition);
                let bound = condition_bindings(&if_stmt.condition);
                self.in_scope(bound, |s| s.visit_block(&if_stmt.then_block));
                let Some(else_block) = &if_stmt.else_block else {
                    return;
                };
                match else_block.stmts.as_slice() {
                    [nested @ Stmt::If(_)] => self.visit_stmt(nested),
                    _ => self.visit_block(else_block),
                }
            }
            Stmt::While(while_stmt) => {
                self.visit_condition(&while_stmt.condition);
                let bound = condition_bindings(&while_stmt.condition);
                self.in_scope(bound, |s| s.visit_block(&while_stmt.body));
            }
            Stmt::For(for_stmt) => {
                let bound = for_stmt.init.as_deref().map_or(Vec::new(), |init| {
                    self.visit_stmt(init);
                    let_bindings(init)
                });
                self.in_scope(bound, |s| {
                    if let Some(condition) = &for_stmt.condition {
                        s.visit_condition(condition);
                    }
                    if let Some(update) = &for_stmt.update {
                        s.visit_expr(update);
                    }
                    s.visit_block(&for_stmt.body);
                });
            }
            Stmt::ForOf(for_of) => {
                self.visit_expr(&for_of.iterable);
                let mut bound = Vec::new();
                pattern_bindings(&for_of.binding, for_of.is_mut, false, &mut bound);
                self.in_scope(bound, |s| s.visit_block(&for_of.body));
            }
            _ => walk_stmt(self, stmt),
        }
    }

    /// An arm's pattern is in scope in its guard and its body, and nowhere else.
    fn visit_match_expr(&mut self, match_expr: &MatchExpr) {
        self.visit_expr(&match_expr.expr);
        for arm in &match_expr.arms {
            let mut bound = Vec::new();
            pattern_bindings(&arm.pattern, false, true, &mut bound);
            self.in_scope(bound, |s| {
                if let Some(guard) = &arm.guard {
                    s.visit_expr(guard);
                }
                s.visit_expr(&arm.body);
            });
        }
    }

    fn visit_block(&mut self, block: &Block) {
        if self.body_depth == 0 {
            walk_block(self, block);
            return;
        }
        self.scopes.push(Vec::new());
        for stmt in &block.stmts {
            // A local item carries its attributes outside its span, so the
            // start offset would land between `#[...]` and the declaration.
            if !matches!(stmt, Stmt::Item(_)) {
                self.sites.push(Site {
                    offset: stmt.span().start,
                    kind: stmt_kind(stmt),
                    mutables: self.visible_mutables(),
                    readables: self.visible_readables(),
                });
            }
            self.visit_stmt(stmt);
            for binding in let_bindings(stmt) {
                self.scopes
                    .last_mut()
                    .expect("a scope is open")
                    .push(binding);
            }
        }
        self.scopes.pop();
    }
}

/// Collect the sites of `source`, keeping only those a guard can actually be
/// written at.
///
/// A statement's span is not proof of a position: the parser re-lexes a
/// template interpolation on its own, so a node inside `${…}` carries an offset
/// relative to the fragment rather than to the file, and inserting there splices
/// a guard into unrelated text. Rather than enumerate which spans to distrust,
/// every span is required to start a token, and the survivors are checked
/// against the parser — all at once, since a fixture normally has nothing wrong
/// with it, and site by site only when that fails.
fn injection_sites(source: &str) -> Vec<Site> {
    let starts = token_starts(source);
    let sites: Vec<Site> = SiteCollector::collect(&wado_compiler::parse(source).ast)
        .into_iter()
        .filter(|site| starts.contains(&site.offset))
        .collect();
    if parses(&inject(source, &sites, "")) {
        return sites;
    }
    sites
        .into_iter()
        .filter(|site| parses(&inject(source, std::slice::from_ref(site), "")))
        .collect()
}

/// The offsets a token starts at.
///
/// A site that is not one lands inside a token — inside a string literal, say,
/// where a spliced guard still parses and so survives [`parses`] below. A
/// template string is one token, so this gives up the statement positions
/// inside its `${…}` too rather than tell them from the literal text around
/// them: 6 sites of 23408, against a class of false finding.
fn token_starts(source: &str) -> IndexSet<usize> {
    wado_compiler::lex(source)
        .tokens
        .iter()
        .map(|token| token.span.start)
        .collect()
}

fn parses(source: &str) -> bool {
    wado_compiler::format(source).is_ok()
}

fn inject(source: &str, sites: &[Site], payload: &str) -> String {
    inject_each(source, sites, |_| payload.to_string())
}

/// Insert a guard at each of `sites`, its body written for that site.
///
/// Offsets are consumed back to front so the earlier ones stay valid.
fn inject_each(source: &str, sites: &[Site], payload: impl Fn(&Site) -> String) -> String {
    let mut mutant = source.to_string();
    for site in sites.iter().rev() {
        mutant.insert_str(site.offset, &guard(&payload(site)));
    }
    mutant
}

/// What the dead region does to the live program.
///
/// A payload reaching no binding renders empty, and the site is not one it can
/// be injected at.
struct Payload {
    name: &'static str,
    render: fn(&Site) -> String,
}

/// Ordered by the analysis family each attacks: alias and mod/ref for the
/// write, liveness and escape for the read.
const PAYLOADS: [Payload; 2] = [
    Payload {
        name: "write",
        render: opaque_writes,
    },
    Payload {
        name: "read",
        render: opaque_reads,
    },
];

/// An opaque write to every binding the dead region can name.
///
/// `black_box` is generic and the assignment is the identity, so this needs no
/// type inference.
fn opaque_writes(site: &Site) -> String {
    render(&site.mutables, |name| {
        format!("{name} = builtin::black_box({name});")
    })
}

/// An opaque read of every binding the dead region can name. It demands no
/// mutability, so it reaches sites [`opaque_writes`] leaves empty.
fn opaque_reads(site: &Site) -> String {
    render(&site.readables, |name| {
        format!("builtin::black_box({name});")
    })
}

fn render(names: &[String], one: impl Fn(&String) -> String) -> String {
    names.iter().map(one).collect::<Vec<_>>().join(" ")
}

// ---------------------------------------------------------------------------
// Evaluation
// ---------------------------------------------------------------------------

/// What a program did, restricted to what an injection must not change.
///
/// `stderr` is deliberately absent: an assertion diagnostic and a trap
/// backtrace both carry positions and function indices that move for reasons
/// that are not wrong code.
struct Outcome {
    stdout: String,
    trapped: bool,
    exit_code: Option<i32>,
    /// Test world: whether any exported test failed. The message is not part
    /// of the comparison — a trap backtrace differs between two builds of the
    /// same program — so only the fact is compared, and the text is reported.
    test_failed: bool,
    /// Diagnostic text (stderr, or the test-world failure). Reported, never
    /// compared.
    detail: String,
}

impl Outcome {
    fn differences(&self, other: &Self) -> Vec<String> {
        let mut out = Vec::new();
        if self.stdout != other.stdout {
            out.push(format!(
                "stdout: {:?} -> {:?}",
                truncate(&self.stdout),
                truncate(&other.stdout)
            ));
        }
        if self.trapped != other.trapped {
            out.push(format!("trapped: {} -> {}", self.trapped, other.trapped));
        }
        if self.exit_code != other.exit_code {
            out.push(format!(
                "exit_code: {:?} -> {:?}",
                self.exit_code, other.exit_code
            ));
        }
        if self.test_failed != other.test_failed {
            out.push(format!(
                "test failure: {} -> {} ({})",
                self.test_failed,
                other.test_failed,
                truncate(&other.detail)
            ));
        }
        out
    }
}

fn truncate(text: &str) -> String {
    const LIMIT: usize = 300;
    if text.len() <= LIMIT {
        return text.to_string();
    }
    let mut end = LIMIT;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}… ({} bytes)", &text[..end], text.len())
}

enum Evaluation {
    Ran(Outcome),
    CompileError(String),
    /// The compiler panicked, or the runtime refused to run what it produced.
    /// On a mutant this is a finding, not an exclusion: an injection that is
    /// unreachable at run time must not be able to crash the compiler.
    Crashed(String),
}

/// Compile and run `source`, catching a panic so one bad subject cannot take
/// the campaign down with it.
fn evaluate(path: &Path, source: &str, spec: &Spec, opt_level: OptLevel) -> Evaluation {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run_once(path, source, spec, opt_level)
    }));
    match result {
        Ok(evaluation) => evaluation,
        Err(payload) => Evaluation::Crashed(format!("panic: {}", panic_message(payload.as_ref()))),
    }
}

fn run_once(path: &Path, source: &str, spec: &Spec, opt_level: OptLevel) -> Evaluation {
    let options = spec.compiler_options(opt_level);
    let compiled = match common::compile_source_with_compiler_options(path, source, options) {
        Ok(compiled) => compiled,
        Err(e) => return Evaluation::CompileError(e.to_string()),
    };

    if spec.test_world {
        // A constant id keeps the runner's messages comparable across runs.
        return match common::run_test_world(
            &compiled.wasm,
            "emi",
            indexmap::IndexMap::new(),
            indexmap::IndexMap::new(),
        ) {
            Ok(result) => Evaluation::Ran(Outcome {
                stdout: result.stdout,
                trapped: result.trapped,
                exit_code: result.exit_code,
                test_failed: false,
                detail: String::new(),
            }),
            Err(e) => Evaluation::Ran(Outcome {
                stdout: String::new(),
                trapped: false,
                exit_code: None,
                test_failed: true,
                detail: format!("{e:#}"),
            }),
        };
    }

    match common::run_wasm(compiled.wasm) {
        Ok(result) => Evaluation::Ran(Outcome {
            stdout: result.stdout,
            trapped: result.trapped,
            exit_code: result.exit_code,
            test_failed: false,
            detail: result.stderr,
        }),
        Err(e) => Evaluation::Crashed(format!("runtime error: {e}")),
    }
}

fn panic_message(payload: &dyn std::any::Any) -> String {
    payload
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| payload.downcast_ref::<&str>().copied())
        .unwrap_or("(non-string panic payload)")
        .to_string()
}

// ---------------------------------------------------------------------------
// Calibration
// ---------------------------------------------------------------------------

/// A source that survived calibration, with the number of guards it accepted.
struct Eligible {
    name: String,
    sites: usize,
}

/// A mutant that misbehaved in a way an injection is not allowed to.
struct Finding {
    name: String,
    kind: &'static str,
    detail: String,
}

/// Re-run the baseline: reached only on a divergence, so a source whose own
/// output moves is not charged to the guard.
fn baseline_moved(
    path: &Path,
    canonical: &str,
    spec: &Spec,
    level: OptLevel,
    first: &Outcome,
) -> Option<String> {
    match evaluate(path, canonical, spec, level) {
        Evaluation::Ran(second) => {
            let differences = first.differences(&second);
            (!differences.is_empty()).then(|| differences.join("; "))
        }
        Evaluation::CompileError(detail) | Evaluation::Crashed(detail) => {
            Some(format!("the baseline stopped running: {detail}"))
        }
    }
}

/// Delta-debug `sites` to a subset that still diverges.
///
/// A mutant carries every site at once — one site per compile is tens of
/// thousands of runs — so this is what makes a finding readable.
fn reduce(mut sites: Vec<Site>, diverges: &dyn Fn(&[Site]) -> bool) -> Vec<Site> {
    let mut granularity = 2;
    while sites.len() > 1 {
        let chunk = sites.len().div_ceil(granularity);
        let bounds: Vec<(usize, usize)> = (0..sites.len())
            .step_by(chunk)
            .map(|start| (start, (start + chunk).min(sites.len())))
            .collect();

        if let Some(part) = bounds
            .iter()
            .map(|&(start, end)| sites[start..end].to_vec())
            .find(|part| diverges(part))
        {
            sites = part;
            granularity = 2;
            continue;
        }

        let complement = bounds
            .iter()
            .map(|&(start, end)| {
                let mut rest = sites[..start].to_vec();
                rest.extend_from_slice(&sites[end..]);
                rest
            })
            .find(|rest| !rest.is_empty() && diverges(rest));

        if let Some(rest) = complement {
            granularity = (granularity - 1).max(2);
            sites = rest;
            continue;
        }

        if granularity >= sites.len() {
            break;
        }
        granularity = (granularity * 2).min(sites.len());
    }
    sites
}

/// Delta-debug a finding's injection set and describe what is left.
fn narrow(
    canonical: &str,
    sites: Vec<Site>,
    reproduces: &dyn Fn(&[Site]) -> bool,
) -> (Vec<Site>, String) {
    let total = sites.len();
    let reduced = reduce(sites, reproduces);
    let detail = format!(
        "reduced to {} of {total} sites at {}",
        reduced.len(),
        site_positions(canonical, &reduced)
    );
    (reduced, detail)
}

/// `line:column kind` for each site, on one line.
fn site_positions(source: &str, sites: &[Site]) -> String {
    sites
        .iter()
        .map(|site| {
            let before = &source[..site.offset];
            let line = before.matches('\n').count() + 1;
            let column = before.len() - before.rfind('\n').map_or(0, |i| i + 1) + 1;
            format!("{line}:{column} {}", site.kind)
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// What a mutant did that an unreachable guard is not allowed to do.
#[derive(Clone, Copy, PartialEq)]
enum Misbehaviour {
    Diverged,
    Crashed,
}

/// The sites `payload` has something to say at.
fn sites_for(sites: &[Site], payload: &Payload) -> Vec<Site> {
    sites
        .iter()
        .filter(|site| !(payload.render)(site).is_empty())
        .cloned()
        .collect()
}

fn is_finding(excluded: &Excluded) -> bool {
    matches!(
        excluded,
        Excluded::GuardCrashed { .. } | Excluded::GuardChangedOutput { .. }
    )
}

/// Inject each payload at every site it reaches, and compare the result against
/// the program it came from.
///
/// A payload the compiler refuses is dropped rather than the source, which
/// stays a subject as long as one payload survives.
fn mutate(subject: &Source, source: &str) -> Result<Eligible, Excluded> {
    let name = subject.name();
    let spec = Spec::parse(subject.root, source)?;
    let canonical =
        wado_compiler::format(source).map_err(|e| Excluded::FormatFailed(e.to_string()))?;

    let all = injection_sites(&canonical);
    let mut alive: Vec<&Payload> = PAYLOADS
        .iter()
        .filter(|payload| !sites_for(&all, payload).is_empty())
        .collect();
    if alive.is_empty() {
        return Err(Excluded::NoBindingInScope);
    }
    let mut refused = None;
    let path = &subject.path;

    for level in CALIBRATION_LEVELS {
        let baseline = match evaluate(path, &canonical, &spec, level) {
            Evaluation::Ran(outcome) => outcome,
            Evaluation::CompileError(detail) => {
                return Err(Excluded::BaselineCompileFailed { level, detail });
            }
            Evaluation::Crashed(detail) => {
                return Err(Excluded::BaselineUnhealthy { level, detail });
            }
        };
        let mut survivors = Vec::new();
        for payload in alive {
            let sites = sites_for(&all, payload);
            match mutate_once(
                path, &canonical, &spec, level, &baseline, payload, sites, &name,
            ) {
                Ok(()) => survivors.push(payload),
                Err(excluded) if is_finding(&excluded) => return Err(excluded),
                Err(excluded) => refused = Some(excluded),
            }
        }
        alive = survivors;
        if alive.is_empty() {
            return Err(refused.expect("a payload that dropped out left its reason"));
        }
    }

    let covered = all
        .iter()
        .filter(|site| {
            alive
                .iter()
                .any(|payload| !(payload.render)(site).is_empty())
        })
        .count();
    Ok(Eligible {
        name,
        sites: covered,
    })
}

/// Run one payload at one level: inject it at every site at once, and reduce
/// what misbehaves back to the guards that cause it.
#[expect(clippy::too_many_arguments, reason = "one call site, all of it needed")]
fn mutate_once(
    path: &Path,
    canonical: &str,
    spec: &Spec,
    level: OptLevel,
    baseline: &Outcome,
    payload: &Payload,
    sites: Vec<Site>,
    name: &str,
) -> Result<(), Excluded> {
    let reproduces = |subset: &[Site], what: Misbehaviour| {
        let mutant = inject_each(canonical, subset, payload.render);
        match evaluate(path, &mutant, spec, level) {
            Evaluation::Ran(outcome) => {
                what == Misbehaviour::Diverged && !baseline.differences(&outcome).is_empty()
            }
            Evaluation::Crashed(_) => what == Misbehaviour::Crashed,
            Evaluation::CompileError(_) => false,
        }
    };
    let report = |detail: String| format!("{} payload: {detail}", payload.name);

    let mutant = inject_each(canonical, &sites, payload.render);
    match evaluate(path, &mutant, spec, level) {
        Evaluation::Ran(outcome) => {
            let differences = baseline.differences(&outcome);
            if !differences.is_empty() {
                if let Some(detail) = baseline_moved(path, canonical, spec, level, baseline) {
                    return Err(Excluded::Nondeterministic {
                        level,
                        detail: report(detail),
                    });
                }
                let (reduced, narrowed) = narrow(canonical, sites, &|subset| {
                    reproduces(subset, Misbehaviour::Diverged)
                });
                write_finding(
                    &format!("{}-{name}", payload.name),
                    &inject_each(canonical, &reduced, payload.render),
                );
                return Err(Excluded::GuardChangedOutput {
                    level,
                    detail: report(format!("{narrowed} — {}", differences.join("; "))),
                });
            }
        }
        Evaluation::CompileError(detail) => {
            return Err(Excluded::GuardRejected {
                level,
                detail: report(detail),
            });
        }
        Evaluation::Crashed(detail) => {
            let (reduced, narrowed) = narrow(canonical, sites, &|subset| {
                reproduces(subset, Misbehaviour::Crashed)
            });
            write_finding(
                &format!("{}-{name}", payload.name),
                &inject_each(canonical, &reduced, payload.render),
            );
            return Err(Excluded::GuardCrashed {
                level,
                detail: report(format!("{narrowed} — {detail}")),
            });
        }
    }

    Ok(())
}

/// Write the reduced mutant so a finding can be read, and re-run, as source.
fn write_finding(name: &str, mutant: &str) {
    let dir = out_dir().join("findings");
    std::fs::create_dir_all(&dir)
        .unwrap_or_else(|e| panic!("cannot create {}: {e}", dir.display()));
    std::fs::write(dir.join(flat_name(name)), mutant).expect("cannot write the reduced mutant");
}

/// A corpus name as one path component, for a file named after a source.
fn flat_name(name: &str) -> String {
    name.replace('/', "-")
}

fn calibrate(subject: &Source, source: &str) -> Result<Eligible, Excluded> {
    let name = subject.name();
    let path = &subject.path;
    let spec = Spec::parse(subject.root, source)?;
    if wado_compiler::parse(source).ast.has_todo() || source.contains("#[TODO]") {
        return Err(Excluded::Todo);
    }

    // The baseline is the source as the formatter renders it, not the file on
    // disk: mutants are produced the same way, so any difference the formatter
    // itself introduces is charged to the formatter and not to the optimizer.
    let canonical =
        wado_compiler::format(source).map_err(|e| Excluded::FormatFailed(e.to_string()))?;

    let sites = injection_sites(&canonical);
    if sites.is_empty() {
        return Err(Excluded::NoInjectionSite);
    }
    let mutant = inject(&canonical, &sites, "");

    for level in CALIBRATION_LEVELS {
        let baseline = match evaluate(path, &canonical, &spec, level) {
            Evaluation::Ran(outcome) => outcome,
            Evaluation::CompileError(detail) => {
                return Err(Excluded::BaselineCompileFailed { level, detail });
            }
            Evaluation::Crashed(detail) => {
                return Err(Excluded::BaselineUnhealthy { level, detail });
            }
        };
        if baseline.test_failed {
            return Err(Excluded::BaselineUnhealthy {
                level,
                detail: truncate(&baseline.detail),
            });
        }

        match evaluate(path, &mutant, &spec, level) {
            Evaluation::Ran(outcome) => {
                let differences = baseline.differences(&outcome);
                if !differences.is_empty() {
                    if let Some(detail) = baseline_moved(path, &canonical, &spec, level, &baseline)
                    {
                        return Err(Excluded::Nondeterministic { level, detail });
                    }
                    return Err(Excluded::GuardChangedOutput {
                        level,
                        detail: differences.join("; "),
                    });
                }
            }
            // An empty guard is valid wherever a statement is, except where the
            // surrounding value must stay constant; that is a rejection, not a
            // divergence.
            Evaluation::CompileError(detail) => {
                return Err(Excluded::GuardRejected { level, detail });
            }
            Evaluation::Crashed(detail) => {
                return Err(Excluded::GuardCrashed { level, detail });
            }
        }
    }

    Ok(Eligible {
        name,
        sites: sites.len(),
    })
}

// ---------------------------------------------------------------------------
// Campaign
// ---------------------------------------------------------------------------

/// Keep the `k`-th of `n` interleaved slices of `paths`, as `WADO_EMI_SHARD=k/n`.
///
/// Interleaved, so a shard's cost does not depend on where the expensive
/// sources cluster alphabetically.
fn take_shard<T>(items: Vec<T>, spec: &str) -> Vec<T> {
    let (index, count) = spec
        .split_once('/')
        .unwrap_or_else(|| panic!("WADO_EMI_SHARD must read `k/n`, got `{spec}`"));
    let index: usize = index
        .parse()
        .unwrap_or_else(|e| panic!("WADO_EMI_SHARD index `{index}`: {e}"));
    let count: usize = count
        .parse()
        .unwrap_or_else(|e| panic!("WADO_EMI_SHARD count `{count}`: {e}"));
    assert!(
        index < count,
        "WADO_EMI_SHARD index {index} is out of range for {count} shard(s)"
    );
    items
        .into_iter()
        .enumerate()
        .filter(|(i, _)| i % count == index)
        .map(|(_, item)| item)
        .collect()
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..")
}

/// Every `.wado` file under `dir`, the tree below it included when `recursive`.
fn wado_files(dir: &Path, recursive: bool) -> Vec<PathBuf> {
    let entries =
        std::fs::read_dir(dir).unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()));
    let mut paths = Vec::new();
    for entry in entries {
        let path = entry.expect("directory entry").path();
        if path.is_dir() {
            if recursive {
                paths.extend(wado_files(&path, true));
            }
        } else if path.extension().is_some_and(|ext| ext == "wado") {
            paths.push(path);
        }
    }
    paths
}

/// The roots `WADO_EMI_ROOTS` selects by name, all of them by default.
fn selected_roots() -> Vec<Root> {
    let Ok(spec) = std::env::var("WADO_EMI_ROOTS") else {
        return ROOTS.to_vec();
    };
    spec.split(',')
        .map(|name| {
            let name = name.trim();
            ROOTS
                .into_iter()
                .find(|root| root.name() == name)
                .unwrap_or_else(|| panic!("WADO_EMI_ROOTS names no root `{name}`"))
        })
        .collect()
}

fn corpus_sources() -> Vec<Source> {
    let filter = std::env::var("WADO_EMI_FILTER").unwrap_or_default();
    let mut sources: Vec<Source> = selected_roots()
        .into_iter()
        .flat_map(|root| {
            wado_files(&root.dir(), root.is_recursive())
                .into_iter()
                .filter(move |path| root.is_subject(path))
                .map(move |path| Source { path, root })
        })
        .filter(|source| filter.is_empty() || source.name().contains(filter.as_str()))
        .collect();
    sources.sort_by_key(Source::name);
    if let Ok(shard) = std::env::var("WADO_EMI_SHARD") {
        sources = take_shard(sources, &shard);
    }
    if let Ok(limit) = std::env::var("WADO_EMI_LIMIT") {
        let limit: usize = limit.parse().expect("WADO_EMI_LIMIT must be a number");
        sources.truncate(limit);
    }
    sources
}

/// The sources the calibration left in `corpus.txt`, all of them.
///
/// The selection knobs act on the calibration, which is what writes this file,
/// so applying them again here would shard an already-sharded list.
fn corpus_subjects() -> Vec<Source> {
    let path = out_dir().join("corpus.txt");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {} — calibrate first: {e}", path.display()));
    text.lines()
        .filter_map(|line| line.split_whitespace().next())
        .map(Source::from_name)
        .collect()
}

fn jobs() -> usize {
    if let Ok(jobs) = std::env::var("WADO_EMI_JOBS") {
        return jobs.parse().expect("WADO_EMI_JOBS must be a number");
    }
    std::thread::available_parallelism().map_or(1, |n| n.get().saturating_sub(1).max(1))
}

#[derive(Default)]
struct Results {
    eligible: Vec<Eligible>,
    excluded: Vec<(String, Excluded)>,
    findings: Vec<Finding>,
}

/// Silences the panic hook for as long as it is alive.
///
/// The campaign's workers are meant to panic: a mutant that crashes the
/// compiler is a finding, and [`evaluate`] catches it, so the default hook
/// would print a backtrace for every one. Restoring the hook is what keeps a
/// panic raised outside a worker reportable at all.
struct SilencedPanics(Option<Box<dyn Fn(&std::panic::PanicHookInfo<'_>) + Sync + Send + 'static>>);

impl SilencedPanics {
    fn install() -> Self {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        Self(Some(previous))
    }
}

impl Drop for SilencedPanics {
    fn drop(&mut self) {
        if let Some(previous) = self.0.take() {
            std::panic::set_hook(previous);
        }
    }
}

/// Run `stage` over `paths` on a pool of workers, and sort what comes back.
///
/// `is_finding` is where the two stages differ: an empty guard that moves the
/// output disqualifies a source, while a payload that moves it is wrong code.
fn campaign(
    subjects: &[Source],
    stage: impl Fn(&Source, &str) -> Result<Eligible, Excluded> + Sync,
    is_finding: impl Fn(&Excluded) -> bool + Sync,
) -> Results {
    let total = subjects.len();
    assert!(
        total > 0,
        "no sources left after WADO_EMI_ROOTS / WADO_EMI_FILTER / WADO_EMI_SHARD"
    );

    let results = Mutex::new(Results::default());
    let next = AtomicUsize::new(0);
    let done = AtomicUsize::new(0);

    let _silenced = SilencedPanics::install();

    std::thread::scope(|scope| {
        for _ in 0..jobs().min(total) {
            scope.spawn(|| {
                loop {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    let Some(subject) = subjects.get(index) else {
                        break;
                    };
                    let source =
                        std::fs::read_to_string(&subject.path).expect("source is readable");
                    let name = subject.name();

                    let outcome = stage(subject, &source);
                    let finished = done.fetch_add(1, Ordering::Relaxed) + 1;
                    if finished.is_multiple_of(50) {
                        eprintln!("[emi] {finished}/{total}");
                    }

                    let mut results = results.lock().expect("results lock");
                    match outcome {
                        Ok(eligible) => results.eligible.push(eligible),
                        Err(excluded) if is_finding(&excluded) => {
                            results.findings.push(Finding {
                                name,
                                kind: excluded.kind(),
                                detail: excluded.detail(),
                            });
                        }
                        Err(excluded) => results.excluded.push((name, excluded)),
                    }
                }
            });
        }
    });

    // Restore the hook before the verdict: an assertion raised under the
    // workers' silence aborts the process instead of failing the test.
    drop(_silenced);

    let mut results = results.into_inner().expect("results lock");
    results.eligible.sort_by(|a, b| a.name.cmp(&b.name));
    results.excluded.sort_by(|a, b| a.0.cmp(&b.0));
    results.findings.sort_by(|a, b| a.name.cmp(&b.name));
    results
}

/// Calibrate the corpus: keep the sources an empty guard leaves alone.
///
/// `#[ignore]`d because it compiles and runs the whole corpus several times
/// over; run it on demand with `cargo test --test emi -- --ignored --nocapture`.
#[test]
#[ignore = "EMI campaign — minutes to hours over the full corpus"]
fn calibrate_corpus() {
    let subjects = corpus_sources();
    let results = campaign(&subjects, calibrate, |excluded| {
        matches!(excluded, Excluded::GuardCrashed { .. })
    });
    write_corpus(&results);
    write_report(&results, &subjects, "calibration");

    assert!(
        results.findings.is_empty(),
        "an unreachable guard crashed the compiler on {} source(s); see the report",
        results.findings.len()
    );
    assert!(
        !results.eligible.is_empty(),
        "calibration left no sources in the corpus"
    );
}

/// Mutate the calibrated corpus: every dead region writes to and reads every
/// binding it can name, and the program must not notice.
///
/// Reads `corpus.txt`, so the calibration runs first.
#[test]
#[ignore = "EMI campaign — minutes to hours over the calibrated corpus"]
fn mutate_corpus() {
    let subjects = corpus_subjects();
    let results = campaign(&subjects, mutate, is_finding);
    write_report(&results, &subjects, "mutation");

    assert!(
        results.findings.is_empty(),
        "a payload behind an undecidable guard changed {} source(s); see the report",
        results.findings.len()
    );
}

fn out_dir() -> PathBuf {
    std::env::var("WADO_EMI_OUT").map_or_else(
        |_| Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/emi"),
        PathBuf::from,
    )
}

fn write_corpus(results: &Results) {
    let dir = out_dir();
    std::fs::create_dir_all(&dir)
        .unwrap_or_else(|e| panic!("cannot create {}: {e}", dir.display()));
    let mut corpus = String::new();
    for eligible in &results.eligible {
        corpus.push_str(&format!("{} {}\n", eligible.name, eligible.sites));
    }
    std::fs::write(dir.join("corpus.txt"), &corpus).expect("cannot write corpus.txt");
}

fn write_report(results: &Results, subjects: &[Source], stage: &str) {
    let dir = out_dir();
    std::fs::create_dir_all(&dir)
        .unwrap_or_else(|e| panic!("cannot create {}: {e}", dir.display()));

    let total = subjects.len();
    let mut report = String::new();
    let sites: usize = results.eligible.iter().map(|e| e.sites).sum();
    report.push_str(&format!(
        "sources scanned: {total}\neligible: {} ({sites} injection sites)\nexcluded: {}\nfindings: {}\n",
        results.eligible.len(),
        results.excluded.len(),
        results.findings.len(),
    ));
    report.push_str(&per_root(results, subjects));

    if !results.findings.is_empty() {
        report.push_str("\n=== findings ===\n");
        for finding in &results.findings {
            report.push_str(&format!(
                "{} ({}) {}\n",
                finding.name, finding.kind, finding.detail
            ));
        }
    }

    let mut kinds: Vec<&str> = results.excluded.iter().map(|(_, e)| e.kind()).collect();
    kinds.sort_unstable();
    kinds.dedup();
    for kind in kinds {
        let group: Vec<&(String, Excluded)> = results
            .excluded
            .iter()
            .filter(|(_, e)| e.kind() == kind)
            .collect();
        report.push_str(&format!("\n=== {kind} ({}) ===\n", group.len()));
        for (name, excluded) in group {
            report.push_str(&format!("{name}: {}\n", excluded.detail()));
        }
    }

    let path = dir.join(format!("{stage}.txt"));
    std::fs::write(&path, &report).expect("cannot write the report");
    eprintln!(
        "[emi] {} eligible / {total} scanned, {} sites — {}",
        results.eligible.len(),
        sites,
        path.display()
    );
}

/// What each root contributed, so material that pays for its compile time can
/// be told from material that does not.
fn per_root(results: &Results, subjects: &[Source]) -> String {
    let mut out = String::from("\n=== corpus ===\n");
    for root in ROOTS {
        let scanned = subjects.iter().filter(|s| s.root == root).count();
        if scanned == 0 {
            continue;
        }
        let drawn = |name: &str| Source::from_name(name).root == root;
        let eligible: Vec<&Eligible> = results.eligible.iter().filter(|e| drawn(&e.name)).collect();
        let sites: usize = eligible.iter().map(|e| e.sites).sum();
        let excluded = results
            .excluded
            .iter()
            .filter(|(name, _)| drawn(name))
            .count();
        let findings = results.findings.iter().filter(|f| drawn(&f.name)).count();
        out.push_str(&format!(
            "{}: {}/{scanned} eligible ({sites} sites), {excluded} excluded, {findings} finding(s)\n",
            root.name(),
            eligible.len(),
        ));
    }
    out
}

/// One line per site: offset, the line and column it lands on, the statement
/// kind, and the text that follows it. A site whose text does not look like the
/// statement it claims to be is a span the collector should not have trusted.
fn describe_sites(source: &str, sites: &[Site]) -> String {
    let mut out = String::new();
    for site in sites {
        let before = &source[..site.offset];
        let line = before.matches('\n').count() + 1;
        let column = before.len() - before.rfind('\n').map_or(0, |i| i + 1) + 1;
        let tail: String = source[site.offset..].chars().take(48).collect();
        out.push_str(&format!(
            "{:>7}  {line}:{column}  {:<14} {:?}\n",
            site.offset, site.kind, tail
        ));
    }
    out
}

/// Write the canonical form and the empty-guard mutant of every source
/// `WADO_EMI_FILTER` selects, so a rejection or a divergence can be read as
/// source instead of inferred from a line and column.
///
/// ```sh
/// WADO_EMI_FILTER=if_merged cargo test --test emi -- --ignored --nocapture dump_mutants
/// ```
#[test]
#[ignore = "inspection aid — writes files, asserts nothing"]
fn dump_mutants() {
    let dir = out_dir().join("dump");
    for subject in corpus_sources() {
        let name = flat_name(&subject.name());
        let source = std::fs::read_to_string(&subject.path).expect("source is readable");
        let Ok(canonical) = wado_compiler::format(&source) else {
            eprintln!("[emi] {name}: the formatter rejected it");
            continue;
        };
        let sites = injection_sites(&canonical);
        let mutant = inject(&canonical, &sites, "");

        let into = dir.join(&name);
        std::fs::create_dir_all(&into)
            .unwrap_or_else(|e| panic!("cannot create {}: {e}", into.display()));
        std::fs::write(into.join("canonical.wado"), &canonical).expect("cannot write canonical");
        std::fs::write(into.join("mutant.wado"), &mutant).expect("cannot write mutant");
        std::fs::write(into.join("sites.txt"), describe_sites(&canonical, &sites))
            .expect("cannot write sites");
        eprintln!("[emi] {name}: {} sites — {}", sites.len(), into.display());
    }
}

// ---------------------------------------------------------------------------
// Harness self-tests
// ---------------------------------------------------------------------------

#[test]
fn guard_is_single_line() {
    assert!(!guard("let x = 1;").contains('\n'));
}

#[test]
fn sites_cover_statements_in_bodies_only() {
    let source = r#"global G: i32 = 1;

fn f() -> i32 {
    let a = 1;
    return a;
}

test "t" {
    assert true;
}
"#;
    let sites = injection_sites(source);
    let kinds: Vec<&str> = sites.iter().map(|site| site.kind).collect();
    assert_eq!(kinds, vec!["let", "return", "assert"]);
}

/// A node inside a template interpolation carries a fragment-relative offset
/// (the parser re-lexes `${…}` on its own), so its span is not a position in
/// the file. Such a site must not reach the mutant.
#[test]
fn interpolation_relative_spans_are_dropped() {
    let source = r#"use { println, Stdout } from "core:cli";

fn f(n: i32) with Stdout {
    println(`v: ${if n > 0 { `pos` } else { `neg` }}`);
}
"#;
    let mutant = inject(source, &injection_sites(source), "");
    assert!(
        wado_compiler::format(&mutant).is_ok(),
        "a mutant must still parse, got:\n{mutant}"
    );
}

/// A guard in front of the `if` an `else if` desugars to would split the chain,
/// so that one position is not a site — while the branches it joins still are.
#[test]
fn else_if_keyword_is_not_a_site() {
    let source = r#"fn f(n: i32) -> i32 {
    if n < 10 {
        return 1;
    } else if n < 20 {
        return 2;
    } else {
        return 3;
    }
}
"#;
    let kinds: Vec<&str> = injection_sites(source).iter().map(|s| s.kind).collect();
    assert_eq!(
        kinds,
        vec!["if", "return", "return", "return"],
        "the statement the chain starts with and each arm's body, but not the \
         `if` an `else if` holds"
    );
}

#[test]
fn injection_preserves_line_count_and_parses() {
    let source = r#"fn f() -> i32 {
    let a = 1;
    return a;
}
"#;
    let sites = injection_sites(source);
    let mutant = inject(source, &sites, "");
    assert_eq!(source.lines().count(), mutant.lines().count());
    assert_eq!(mutant.matches("black_box").count(), sites.len());
    wado_compiler::format(&mutant).expect("a mutant must still parse");
}

#[test]
fn unsupported_data_key_leaves_the_corpus() {
    let source = "fn f() {}\n\n__DATA__\n{\"stdin\": \"x\"}\n";
    let excluded = Spec::parse(Root::Fixtures, source)
        .err()
        .expect("stdin is not understood");
    assert_eq!(excluded.kind(), "unsupported __DATA__ key");
}

#[test]
fn missing_data_section_runs_the_root_s_world() {
    let fixture = Spec::parse(Root::Fixtures, "fn f() {}\n").expect("a fixture is eligible");
    assert!(fixture.test_world);
    let example = Spec::parse(Root::Example, "fn f() {}\n").expect("an example is eligible");
    assert!(
        !example.test_world,
        "an example exports `run()` for the CLI world"
    );
}

/// The corpus spans roots, so a bare file name no longer identifies a source —
/// `json_test.wado` is both a fixture and a stdlib module.
#[test]
fn a_corpus_name_names_its_root() {
    let source = Source {
        path: Root::Stdlib.dir().join("core/json_test.wado"),
        root: Root::Stdlib,
    };
    assert_eq!(source.name(), "wado-compiler/lib/core/json_test.wado");
    let recovered = Source::from_name(&source.name());
    assert_eq!(recovered.root, Root::Stdlib);
    assert_eq!(recovered.path, source.path);
}

/// Every root must reach the disk: a typo in a directory would otherwise show
/// up as a quietly smaller corpus.
#[test]
fn every_root_draws_material() {
    for root in ROOTS {
        assert!(
            !wado_files(&root.dir(), root.is_recursive()).is_empty(),
            "{} drew nothing from {}",
            root.name(),
            root.dir().display()
        );
    }
}

/// A stdlib module is material only if the test world runs something in it.
#[test]
fn a_stdlib_module_is_material_only_with_a_test_block() {
    let dir = Root::Stdlib.dir();
    assert!(
        Root::Stdlib.is_subject(&dir.join("core/base64_test.wado")),
        "a module with `test` blocks is a subject"
    );
    assert!(
        !Root::Stdlib.is_subject(&dir.join("core/rt.wado")),
        "a module the test world runs nothing in cannot show a divergence"
    );
}

/// An example is material only if a world enters it — otherwise its baseline
/// fails to compile and the report cannot tell that apart from a regression.
#[test]
fn an_example_is_material_only_if_a_world_enters_it() {
    let dir = Root::Example.dir();
    assert!(
        Root::Example.is_subject(&dir.join("fizzbuzz.wado")),
        "a program with `export fn run` is a subject"
    );
    assert!(
        !Root::Example.is_subject(&dir.join("router_common.wado")),
        "a library the CLI world cannot enter is not material"
    );
    assert!(
        !Root::Example.is_subject(&dir.join("http_server.wado")),
        "a `wasi:http/service` program is not one the CLI world enters"
    );
}

/// The premise of drawing from the stdlib: a module compiled from its own path,
/// rather than as the built-in `core:*` it also is, still runs.
#[test]
fn a_stdlib_module_runs_from_its_own_path() {
    let subject = Source {
        path: Root::Stdlib.dir().join("core/base64_test.wado"),
        root: Root::Stdlib,
    };
    let source = std::fs::read_to_string(&subject.path).expect("source is readable");
    let spec = Spec::parse(subject.root, &source).expect("the stdlib carries no __DATA__");
    match evaluate(&subject.path, &source, &spec, OptLevel::O0) {
        Evaluation::Ran(outcome) => assert!(
            !outcome.test_failed,
            "the stdlib's own tests must pass: {}",
            outcome.detail
        ),
        Evaluation::CompileError(detail) | Evaluation::Crashed(detail) => {
            panic!("a stdlib module must compile from its own path: {detail}")
        }
    }
}

/// The mutation stage reports a divergence as a finding, so a source whose own
/// output moves has to be told apart from a guard's doing before it becomes one.
#[test]
fn nondeterminism_is_told_apart_from_a_guard_divergence() {
    let source = r#"use { println, Stdout } from "core:cli";
use { MonotonicClock } from "wasi:clocks";

export fn run() with (Stdout, MonotonicClock) {
    let t = MonotonicClock::now();
    println(`${t}`);
}

__DATA__
{}
"#;
    let subject = Source {
        path: Root::Fixtures.dir().join("emi_clock_nondeterminism.wado"),
        root: Root::Fixtures,
    };
    let excluded = calibrate(&subject, source)
        .err()
        .expect("a printed clock reading cannot be an oracle");
    assert_eq!(excluded.kind(), "source is nondeterministic");
}

/// `WADO_EMI_FILTER` selects by substring and `WADO_EMI_LIMIT` truncates the
/// sorted list, so neither can express "the k-th of n". The matrix needs that.
#[test]
fn shards_partition_the_corpus() {
    let paths: Vec<PathBuf> = (0..10)
        .map(|i| PathBuf::from(format!("{i}.wado")))
        .collect();
    let shards: Vec<Vec<PathBuf>> = (0..4)
        .map(|k| take_shard(paths.clone(), &format!("{k}/4")))
        .collect();

    assert_eq!(shards[0], ["0.wado", "4.wado", "8.wado"].map(PathBuf::from));
    let mut union = shards.concat();
    union.sort();
    assert_eq!(union, paths, "every source lands in exactly one shard");
}

/// A statement's span is a claim about the AST, not about the text. An offset
/// that lands inside a string literal still parses once a guard is spliced in,
/// so the parse check cannot see it; a token start can.
#[test]
fn a_position_inside_a_string_literal_is_not_a_token_start() {
    let source = "fn f() -> String {\n    return \"return x;\";\n}\n";
    let starts = token_starts(source);
    assert!(starts.contains(&source.find("return").expect("the statement")));
    assert!(!starts.contains(&source.find("return x;").expect("the text in the literal")));
}

#[test]
fn every_site_is_a_token_start() {
    let source = r#"use { println, Stdout } from "core:cli";

fn f(n: i32) with Stdout {
    println("return n;");
    let s = "let x = 1;";
    println(s);
}
"#;
    let starts = token_starts(source);
    for site in injection_sites(source) {
        assert!(
            starts.contains(&site.offset),
            "site at {} lands inside a token",
            site.offset
        );
    }
}

/// The payload writes to the bindings the dead region can name, so a site has
/// to know which `let mut` are live where it stands — and which have gone out
/// of scope again.
#[test]
fn a_site_sees_the_mutable_bindings_in_scope() {
    let source = r#"fn f(n: i32) -> i32 {
    let mut a = 0;
    let b = 1;
    if n > 0 {
        let mut c = 2;
        a = a + c;
    }
    return a + b;
}
"#;
    let seen: Vec<(&str, Vec<String>)> = injection_sites(source)
        .iter()
        .map(|site| (site.kind, site.mutables.clone()))
        .collect();
    let expected: Vec<(&str, Vec<String>)> = vec![
        ("let", vec![]),
        ("let", vec!["a".into()]),
        ("if", vec!["a".into()]),
        ("let", vec!["a".into()]),
        ("expr", vec!["a".into(), "c".into()]),
        ("return", vec!["a".into()]),
    ];
    assert_eq!(seen, expected);
}

/// The read payload demands no mutability, so a site has to know every binding
/// it may name and not only the assignable ones.
#[test]
fn a_site_sees_the_readable_bindings_in_scope() {
    let source = r#"fn f(n: i32) -> i32 {
    let a = 1;
    let mut b = 2;
    return n + a + b;
}
"#;
    let seen: Vec<(Vec<String>, Vec<String>)> = injection_sites(source)
        .iter()
        .map(|site| (site.mutables.clone(), site.readables.clone()))
        .collect();
    let expected: Vec<(Vec<String>, Vec<String>)> = vec![
        (vec![], vec!["n".into()]),
        (vec![], vec!["n".into(), "a".into()]),
        (vec!["b".into()], vec!["n".into(), "a".into(), "b".into()]),
    ];
    assert_eq!(seen, expected);
}

/// A name reaches the binding that shadows it, so an outer `let mut` an inner
/// `let` shadows is not assignable — writing to it would not compile.
#[test]
fn a_shadowed_binding_is_answered_for_by_the_one_that_shadows_it() {
    let source = r#"fn f() -> i32 {
    let mut x = 1;
    if x > 0 {
        let x = x + 10;
        assert x == 11;
    }
    return x;
}
"#;
    let site = injection_sites(source)
        .into_iter()
        .find(|site| site.kind == "assert")
        .expect("the assert is a site");
    assert!(site.mutables.is_empty(), "{:?}", site.mutables);
    assert_eq!(site.readables, vec!["x"]);
}

/// A destructuring pattern binds names the same way a plain `let` does, and a
/// `mut` leaf inside one is assignable.
#[test]
fn a_destructured_binding_is_named_by_its_leaves() {
    let source = r#"fn f(p: [i32, i32]) -> i32 {
    let [a, mut b] = p;
    return a + b;
}
"#;
    let last = injection_sites(source)
        .pop()
        .expect("the return statement is a site");
    assert_eq!(last.mutables, vec!["b".to_string()]);
    assert_eq!(last.readables, vec!["p", "a", "b"]);
}

/// A loop, a `match` arm and an `if let` each bind over a body, and those
/// bodies are where the passes the payloads attack do their work.
#[test]
fn a_binding_a_body_is_entered_with_is_in_scope_inside_it() {
    let source = r#"fn f(l: List<i32>, o: Option<i32>) -> i32 {
    for let x of l {
        assert x >= 0;
    }
    match o {
        Some(v) => { assert v >= 0; },
        None => { assert true; },
    }
    if let Some(w) = o {
        assert w >= 0;
    }
    return 0;
}
"#;
    let inner: Vec<Vec<String>> = injection_sites(source)
        .iter()
        .filter(|site| site.kind == "assert")
        .map(|site| site.readables.clone())
        .collect();
    assert_eq!(
        inner,
        vec![
            vec!["l", "o", "x"],
            vec!["l", "o", "v"],
            // `None` parses as a plain name and is a variant, not a binding.
            vec!["l", "o"],
            vec!["l", "o", "w"],
        ]
    );
}

/// A closure may read what it captured — that is just a use — but assigning to
/// a capture promotes it to `fn mut` and every call through a plain `let` then
/// stops compiling, so the write payload stays inside the closure's own scope.
#[test]
fn a_closure_reads_its_captures_and_writes_only_its_own_bindings() {
    let source = r#"fn f(mut n: i32) -> i32 {
    let g = |mut k: i32| -> i32 {
        return k + n;
    };
    return g(1);
}
"#;
    let inside = injection_sites(source)
        .into_iter()
        .find(|site| site.kind == "return" && site.readables.contains(&"k".to_string()))
        .expect("the closure body has a site");
    assert_eq!(inside.mutables, vec!["k".to_string()]);
    assert_eq!(
        inside.readables,
        vec!["n", "k"],
        "`g` is not in scope inside its own initializer"
    );
}

/// The payload the roadmap adds: it reaches a site the write payload leaves
/// empty, which is most of the corpus.
#[test]
fn the_read_payload_reaches_a_site_the_write_payload_cannot() {
    let source = "fn f(n: i32) -> i32 {\n    return n;\n}\n";
    let site = &injection_sites(source)[0];
    assert_eq!(opaque_writes(site), "");
    assert_eq!(opaque_reads(site), "builtin::black_box(n);");
}

/// A finding arrives as a mutant carrying every site at once, so the reduction
/// is what names the guards that actually moved the output.
#[test]
fn reduction_keeps_only_the_sites_that_matter() {
    let sites: Vec<Site> = (0..16)
        .map(|offset| Site {
            offset,
            kind: "let",
            mutables: Vec::new(),
            readables: Vec::new(),
        })
        .collect();
    let culprits = [3, 11];
    let reduced = reduce(sites, &|subset| {
        culprits
            .iter()
            .all(|c| subset.iter().any(|site| site.offset == *c))
    });
    assert_eq!(
        reduced.iter().map(|site| site.offset).collect::<Vec<_>>(),
        culprits
    );
}

/// A `mut` parameter is a binding the body can write to just as much as a
/// `let mut`, and for a small function it is often the only one.
#[test]
fn a_mut_parameter_is_in_scope_from_the_first_site() {
    let source = "fn f(mut n: i32, k: i32) -> i32 {\n    n = n + k;\n    return n;\n}\n";
    let seen: Vec<Vec<String>> = injection_sites(source)
        .iter()
        .map(|site| site.mutables.clone())
        .collect();
    assert_eq!(seen, vec![vec!["n".to_string()], vec!["n".to_string()]]);
}

/// A finding arrives as a mutant carrying every site; the guards left after
/// narrowing are the ones a person has to read.
#[test]
fn narrowing_names_the_guards_that_carry_a_finding() {
    let source = "fn f() {\n    let a = 1;\n    let b = 2;\n    let c = 3;\n}\n";
    let sites: Vec<Site> = [13, 28, 43]
        .into_iter()
        .map(|offset| Site {
            offset,
            kind: "let",
            mutables: Vec::new(),
            readables: Vec::new(),
        })
        .collect();

    let (reduced, detail) = narrow(source, sites, &|subset| {
        subset.iter().any(|site| site.offset == 28)
    });

    assert_eq!(reduced.len(), 1);
    assert_eq!(reduced[0].offset, 28);
    assert!(detail.contains("1 of 3 sites"), "{detail}");
    assert!(detail.contains("3:5 let"), "{detail}");
}
