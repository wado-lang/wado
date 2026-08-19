//! EMI (equivalence modulo inputs) harness for the optimizer.
//!
//! `builtin::black_box(false)` is a condition no pass can decide: the NIR
//! optimizer treats the call as opaque and `wir_build` emits the argument
//! where the call stood, so a block behind such a guard is unreachable at run
//! time, visible to every NIR pass, and absent from the emitted Wasm. Injecting
//! one into a working program must therefore leave the program's output
//! untouched — a difference is a wrong-code bug.
//!
//! [`calibrate_corpus`] carries the calibration stage. It injects an *empty*
//! guard at every statement boundary of every fixture and keeps the ones whose
//! observable behaviour survives. Guards are written on a single line, so the
//! code after an injection keeps the line numbers it had and a fixture that
//! prints an assertion diagnostic is not disturbed; what calibration still
//! catches is a fixture that reads a column, an allocation address, or a
//! generated test-export name, and one whose output moves between runs of the
//! same program. Those cannot serve as an EMI oracle, and naming them here is
//! what keeps a later divergence from being mistaken for one.
//!
//! The eligible names are written to `target/emi/corpus.txt` for the mutation
//! stage to consume; every exclusion lands in `target/emi/calibration.txt`
//! with its reason.
//!
//! [`mutate_corpus`] carries the mutation stage: each guard writes to every
//! `let mut` in scope, so the dead region touches the live program and the
//! alias and mod/ref analyses behind `licm`, `store_load_forward`,
//! `field_scalarize`, `copy_prop` and `sroa` have to survive it. A divergence
//! is delta-debugged back to the guards that caused it.
//!
//! ```sh
//! cargo test --test emi -- --ignored --nocapture
//! ```
//!
//! Knobs: `WADO_EMI_JOBS`, `WADO_EMI_FILTER`, `WADO_EMI_SHARD` (`k/n`),
//! `WADO_EMI_LIMIT`, `WADO_EMI_OUT`.
//!
//! ## Next
//!
//! - [ ] Payload: statements harvested from elsewhere in the same function,
//!   type-correct by construction wherever their free variables are in scope.
//! - [ ] `while builtin::black_box(false) { … }` as a second guard shape, for
//!   the loop passes.
//! - [ ] Name the pass behind a finding by bisecting `WADO_LIST_PASSES` with
//!   `WADO_SKIP_PASS`, and write the reduced program out as a fixture.

mod common;

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use wado_compiler::ast::{
    AstVisitor, Block, Expr, Function, Item, Module, Pattern, SelfKind, Stmt, walk_block,
    walk_expr, walk_function, walk_item, walk_stmt,
};
use wado_compiler::hashmap::IndexSet;
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
/// or every fixture that reports a source line would drop out of the corpus.
fn guard(payload: &str) -> String {
    format!("if builtin::black_box(false) {{ {payload} }} ")
}

// ---------------------------------------------------------------------------
// Corpus
// ---------------------------------------------------------------------------

/// Is this `__DATA__` key one the harness understands?
///
/// `test` and `allocator` select the world and the allocator. Every other
/// understood key states an expectation, which the baseline run supersedes —
/// EMI compares a mutant against the program it came from, not against the
/// fixture's recorded output. A key that is *not* understood either feeds the
/// runner an input the comparison does not reproduce (a request, a preopen,
/// stdin, a compile-time parameter) or was added after this list was written;
/// either way the fixture leaves the corpus instead of being run with the input
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

/// How a fixture must be compiled and run.
struct Spec {
    test_world: bool,
    allocator: String,
}

impl Spec {
    /// A fixture with no `__DATA__` section is a library-shaped source run
    /// under the test world, matching the e2e harness.
    fn parse(source: &str) -> Result<Self, Excluded> {
        let Some(data) = common::extract_data_section(source) else {
            return Ok(Self {
                test_world: true,
                allocator: "debug".to_string(),
            });
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

/// Why a fixture cannot serve as an EMI oracle.
#[derive(Debug)]
enum Excluded {
    MalformedData(String),
    UnsupportedDataKey(String),
    Todo,
    FormatFailed(String),
    NoInjectionSite,
    /// No `let mut` in scope at any site, so the payload has nothing to write.
    NoMutableInScope,
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
    /// The empty guard moved the program's output — the fixture observes
    /// something an injection perturbs, so a real mutation could not be told
    /// apart from that.
    GuardChangedOutput {
        level: OptLevel,
        detail: String,
    },
    /// The fixture's own output moves between runs, so no mutant can be
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
            Excluded::FormatFailed(_) => "formatter rejected the fixture",
            Excluded::NoInjectionSite => "no injection site",
            Excluded::NoMutableInScope => "no mutable binding in scope",
            Excluded::BaselineCompileFailed { .. } => "baseline failed to compile",
            Excluded::BaselineUnhealthy { .. } => "baseline did not pass",
            Excluded::GuardRejected { .. } => "guard failed to compile",
            Excluded::GuardChangedOutput { .. } => "guard changed the output",
            Excluded::Nondeterministic { .. } => "fixture is nondeterministic",
            Excluded::GuardCrashed { .. } => "guard crashed the compiler",
        }
    }

    fn detail(&self) -> String {
        match self {
            Excluded::MalformedData(d) | Excluded::FormatFailed(d) => d.clone(),
            Excluded::UnsupportedDataKey(k) => k.clone(),
            Excluded::Todo | Excluded::NoInjectionSite | Excluded::NoMutableInScope => {
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
    /// The `let mut` bindings in scope here, which a payload may write to.
    mutables: Vec<String>,
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
    /// `let mut` bindings per open block, innermost last.
    scopes: Vec<Vec<String>>,
}

impl SiteCollector {
    fn collect(module: &Module) -> Vec<Site> {
        let mut collector = Self {
            sites: Vec::new(),
            body_depth: 0,
            scopes: Vec::new(),
        };
        for item in &module.items {
            collector.visit_item(item);
        }
        collector.sites.sort_by_key(|site| site.offset);
        collector.sites.dedup_by_key(|site| site.offset);
        collector.sites
    }

    fn visible_mutables(&self) -> Vec<String> {
        self.scopes.iter().flatten().cloned().collect()
    }

    /// Enter a body with its own scope stack, seeded with its `mut`
    /// parameters, so a nested body does not inherit bindings it cannot name.
    fn in_body(&mut self, params: Vec<String>, walk: impl FnOnce(&mut Self)) {
        self.body_depth += 1;
        let outer = std::mem::take(&mut self.scopes);
        self.scopes.push(params);
        walk(self);
        self.scopes = outer;
        self.body_depth -= 1;
    }
}

/// The name a statement introduces as an assignable binding.
///
/// An uninitialized `let mut x: i32;` is left out: a payload that reads it
/// before its first assignment would not compile.
fn mut_binding(stmt: &Stmt) -> Option<String> {
    let Stmt::Let(let_stmt) = stmt else {
        return None;
    };
    if let_stmt.is_reactive || let_stmt.value.is_none() {
        return None;
    }
    // `let mut x` carries the `mut` on the statement; `MutIdent` is what a
    // nested pattern binds.
    if let Pattern::MutIdent { name, .. } = &let_stmt.pattern {
        return Some(name.clone());
    }
    if let Pattern::Ident { name, .. } = &let_stmt.pattern
        && let_stmt.is_mut
    {
        return Some(name.clone());
    }
    None
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
        // A `self` receiver is not a name the body may assign to.
        let params = func
            .params
            .iter()
            .filter(|param| param.is_mut && param.self_kind == SelfKind::None)
            .map(|param| param.name.clone())
            .collect();
        self.in_body(params, |s| walk_function(s, func));
    }

    /// A closure gets its own scope stack, not for hygiene but for typing: a
    /// payload writing to a binding the closure captured would promote it to
    /// `fn mut`, and every call through a plain `let` then stops compiling.
    fn visit_expr(&mut self, expr: &Expr) {
        let Expr::Closure(closure) = expr else {
            walk_expr(self, expr);
            return;
        };
        let params = closure
            .params
            .iter()
            .filter(|param| param.is_mut)
            .map(|param| param.name.clone())
            .collect();
        self.in_body(params, |s| walk_expr(s, expr));
    }

    /// `else if` is an `else` block holding one `If`, and that `If`'s span
    /// starts at the `if` keyword. Visiting the block would offer the keyword
    /// as a site, and a guard there splits the chain into an `else` that takes
    /// the guard and a stray `if` — so the nested statement is visited
    /// directly, contributing its interior without the position in front of it.
    /// An `else { if … }` written with braces has the same shape and loses that
    /// one site too; the interior sites are unaffected.
    fn visit_stmt(&mut self, stmt: &Stmt) {
        let Stmt::If(if_stmt) = stmt else {
            walk_stmt(self, stmt);
            return;
        };
        self.visit_condition(&if_stmt.condition);
        self.visit_block(&if_stmt.then_block);
        let Some(else_block) = &if_stmt.else_block else {
            return;
        };
        match else_block.stmts.as_slice() {
            [nested @ Stmt::If(_)] => self.visit_stmt(nested),
            _ => self.visit_block(else_block),
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
                });
            }
            self.visit_stmt(stmt);
            if let Some(name) = mut_binding(stmt) {
                self.scopes.last_mut().expect("a scope is open").push(name);
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

/// An opaque write to every binding the dead region can name.
///
/// `black_box` is generic and the assignment is the identity, so this needs no
/// type inference.
fn opaque_writes(site: &Site) -> String {
    site.mutables
        .iter()
        .map(|name| format!("{name} = builtin::black_box({name});"))
        .collect::<Vec<_>>()
        .join(" ")
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

/// Compile and run `source`, catching a panic so one bad fixture cannot take
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

/// A fixture that survived calibration, with the number of guards it accepted.
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

/// Re-run the baseline: reached only on a divergence, so a fixture whose own
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

/// Inject an opaque write to every binding in scope at every site, and compare
/// the result against the program it came from.
fn mutate(path: &Path, source: &str) -> Result<Eligible, Excluded> {
    let name = fixture_name(path);
    let spec = Spec::parse(source)?;
    let canonical =
        wado_compiler::format(source).map_err(|e| Excluded::FormatFailed(e.to_string()))?;

    let sites: Vec<Site> = injection_sites(&canonical)
        .into_iter()
        .filter(|site| !site.mutables.is_empty())
        .collect();
    if sites.is_empty() {
        return Err(Excluded::NoMutableInScope);
    }

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
        let diverges = |subset: &[Site]| {
            let mutant = inject_each(&canonical, subset, opaque_writes);
            match evaluate(path, &mutant, &spec, level) {
                Evaluation::Ran(outcome) => !baseline.differences(&outcome).is_empty(),
                Evaluation::CompileError(_) | Evaluation::Crashed(_) => false,
            }
        };

        let mutant = inject_each(&canonical, &sites, opaque_writes);
        match evaluate(path, &mutant, &spec, level) {
            Evaluation::Ran(outcome) => {
                let differences = baseline.differences(&outcome);
                if !differences.is_empty() {
                    if let Some(detail) = baseline_moved(path, &canonical, &spec, level, &baseline)
                    {
                        return Err(Excluded::Nondeterministic { level, detail });
                    }
                    let reduced = reduce(sites.clone(), &diverges);
                    write_finding(&name, &inject_each(&canonical, &reduced, opaque_writes));
                    return Err(Excluded::GuardChangedOutput {
                        level,
                        detail: format!(
                            "{} — reduced to {} of {} sites at {}",
                            differences.join("; "),
                            reduced.len(),
                            sites.len(),
                            site_positions(&canonical, &reduced)
                        ),
                    });
                }
            }
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

/// Write the reduced mutant so a finding can be read, and re-run, as source.
fn write_finding(name: &str, mutant: &str) {
    let dir = out_dir().join("findings");
    std::fs::create_dir_all(&dir)
        .unwrap_or_else(|e| panic!("cannot create {}: {e}", dir.display()));
    std::fs::write(dir.join(name), mutant).expect("cannot write the reduced mutant");
}

fn calibrate(path: &Path, source: &str) -> Result<Eligible, Excluded> {
    let name = path
        .file_name()
        .expect("fixture path has a file name")
        .to_string_lossy()
        .to_string();

    let spec = Spec::parse(source)?;
    if wado_compiler::parse(source).ast.has_todo() || source.contains("#[TODO]") {
        return Err(Excluded::Todo);
    }

    // The baseline is the fixture as the formatter renders it, not the file on
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
/// fixtures cluster alphabetically.
fn take_shard(paths: Vec<PathBuf>, spec: &str) -> Vec<PathBuf> {
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
    paths
        .into_iter()
        .enumerate()
        .filter(|(i, _)| i % count == index)
        .map(|(_, path)| path)
        .collect()
}

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn fixture_name(path: &Path) -> String {
    path.file_name()
        .expect("fixture path has a file name")
        .to_string_lossy()
        .to_string()
}

fn fixture_paths() -> Vec<PathBuf> {
    let dir = fixtures_dir();
    let filter = std::env::var("WADO_EMI_FILTER").unwrap_or_default();
    let mut paths: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()))
        .map(|entry| entry.expect("directory entry").path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "wado"))
        .filter(|path| filter.is_empty() || path.to_string_lossy().contains(filter.as_str()))
        .collect();
    paths.sort();
    if let Ok(shard) = std::env::var("WADO_EMI_SHARD") {
        paths = take_shard(paths, &shard);
    }
    if let Ok(limit) = std::env::var("WADO_EMI_LIMIT") {
        let limit: usize = limit.parse().expect("WADO_EMI_LIMIT must be a number");
        paths.truncate(limit);
    }
    paths
}

/// The fixtures the calibration left in `corpus.txt`, all of them.
///
/// The selection knobs act on the calibration, which is what writes this file,
/// so applying them again here would shard an already-sharded list.
fn corpus_paths() -> Vec<PathBuf> {
    let path = out_dir().join("corpus.txt");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {} — calibrate first: {e}", path.display()));
    let dir = fixtures_dir();
    text.lines()
        .filter_map(|line| line.split_whitespace().next())
        .map(|name| dir.join(name))
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
/// output disqualifies a fixture, while a payload that moves it is wrong code.
fn campaign(
    paths: &[PathBuf],
    stage: impl Fn(&Path, &str) -> Result<Eligible, Excluded> + Sync,
    is_finding: impl Fn(&Excluded) -> bool + Sync,
) -> Results {
    let total = paths.len();
    assert!(
        total > 0,
        "no fixtures left after WADO_EMI_FILTER / WADO_EMI_SHARD"
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
                    let Some(path) = paths.get(index) else { break };
                    let source = std::fs::read_to_string(path).expect("fixture is readable");
                    let name = fixture_name(path);

                    let outcome = stage(path, &source);
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

/// Calibrate the fixture corpus: keep the fixtures an empty guard leaves alone.
///
/// `#[ignore]`d because it compiles and runs the whole corpus several times
/// over; run it on demand with `cargo test --test emi -- --ignored --nocapture`.
#[test]
#[ignore = "EMI campaign — minutes to hours over the full corpus"]
fn calibrate_corpus() {
    let paths = fixture_paths();
    let results = campaign(&paths, calibrate, |excluded| {
        matches!(excluded, Excluded::GuardCrashed { .. })
    });
    write_corpus(&results);
    write_report(&results, paths.len(), "calibration");

    assert!(
        results.findings.is_empty(),
        "an unreachable guard crashed the compiler on {} fixture(s); see the report",
        results.findings.len()
    );
    assert!(
        !results.eligible.is_empty(),
        "calibration left no fixtures in the corpus"
    );
}

/// Mutate the calibrated corpus: every dead region writes to every binding it
/// can name, and the program must not notice.
///
/// Reads `corpus.txt`, so the calibration runs first.
#[test]
#[ignore = "EMI campaign — hours over the calibrated corpus"]
fn mutate_corpus() {
    let paths = corpus_paths();
    let results = campaign(&paths, mutate, |excluded| {
        matches!(
            excluded,
            Excluded::GuardCrashed { .. } | Excluded::GuardChangedOutput { .. }
        )
    });
    write_report(&results, paths.len(), "mutation");

    assert!(
        results.findings.is_empty(),
        "a payload behind an undecidable guard changed {} fixture(s); see the report",
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

fn write_report(results: &Results, total: usize, stage: &str) {
    let dir = out_dir();
    std::fs::create_dir_all(&dir)
        .unwrap_or_else(|e| panic!("cannot create {}: {e}", dir.display()));

    let mut report = String::new();
    let sites: usize = results.eligible.iter().map(|e| e.sites).sum();
    report.push_str(&format!(
        "fixtures scanned: {total}\neligible: {} ({sites} injection sites)\nexcluded: {}\nfindings: {}\n",
        results.eligible.len(),
        results.excluded.len(),
        results.findings.len(),
    ));

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

/// Write the canonical form and the empty-guard mutant of every fixture
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
    for path in fixture_paths() {
        let name = path
            .file_stem()
            .expect("fixture path has a file name")
            .to_string_lossy()
            .to_string();
        let source = std::fs::read_to_string(&path).expect("fixture is readable");
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
    let excluded = Spec::parse(source).err().expect("stdin is not understood");
    assert_eq!(excluded.kind(), "unsupported __DATA__ key");
}

#[test]
fn missing_data_section_runs_the_test_world() {
    let spec = Spec::parse("fn f() {}\n").expect("a source without __DATA__ is eligible");
    assert!(spec.test_world);
}

/// The mutation stage reports a divergence as a finding, so a fixture whose own
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
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/emi_clock_nondeterminism.wado");
    let excluded = calibrate(&path, source)
        .err()
        .expect("a printed clock reading cannot be an oracle");
    assert_eq!(excluded.kind(), "fixture is nondeterministic");
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
    assert_eq!(union, paths, "every fixture lands in exactly one shard");
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

/// A finding arrives as a mutant carrying every site at once, so the reduction
/// is what names the guards that actually moved the output.
#[test]
fn reduction_keeps_only_the_sites_that_matter() {
    let sites: Vec<Site> = (0..16)
        .map(|offset| Site {
            offset,
            kind: "let",
            mutables: Vec::new(),
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
