//! EMI (equivalence modulo inputs) harness for the optimizer.
//!
//! `builtin::black_box(false)` is a condition no pass can decide: the NIR
//! optimizer treats the call as opaque and `wir_build` emits the argument
//! where the call stood, so a block behind such a guard is unreachable at run
//! time, visible to every NIR pass, and absent from the emitted Wasm. Injecting
//! one into a working program must therefore leave the program's output
//! untouched — a difference is a wrong-code bug.
//!
//! This file carries the campaign's calibration stage. It injects an *empty*
//! guard at every statement boundary of every fixture and keeps the ones whose
//! observable behaviour survives. Guards are written on a single line, so the
//! code after an injection keeps the line numbers it had and a fixture that
//! prints an assertion diagnostic is not disturbed; what calibration still
//! catches is a fixture that reads a column, an allocation address, or a
//! generated test-export name. Those cannot serve as an EMI oracle, and naming
//! them here is what keeps a later divergence from being mistaken for one.
//!
//! The eligible names are written to `target/emi/corpus.txt` for the mutation
//! stages to consume; every exclusion lands in `target/emi/calibration.txt`
//! with its reason.
//!
//! ```sh
//! cargo test --test emi -- --ignored --nocapture
//! ```
//!
//! Knobs: `WADO_EMI_JOBS`, `WADO_EMI_FILTER`, `WADO_EMI_LIMIT`, `WADO_EMI_OUT`.
//!
//! ## Next
//!
//! An empty guard only changes the shape a pass sees. A guard with a body makes
//! the dead region read and write the live program, which is what the analyses
//! most likely to be wrong — alias, mod/ref, loop — actually rest on.
//!
//! - [ ] Payload: `x = builtin::black_box(x)` for each `let mut` in scope. It
//!   is an opaque write to a real binding and needs no type inference, since
//!   `black_box` is generic and the assignment is the identity. Attacks `licm`,
//!   `store_load_forward`, `field_scalarize`, `copy_prop`, `sroa`.
//! - [ ] Payload: statements harvested from elsewhere in the same function,
//!   type-correct by construction wherever their free variables are in scope.
//! - [ ] `while builtin::black_box(false) { … }` as a second guard shape, for
//!   the loop passes.
//! - [ ] Reduction: delta-debug the injection set to the guard that matters,
//!   then bisect `WADO_LIST_PASSES` with `WADO_SKIP_PASS` to name the pass, and
//!   write the reduced program out as a fixture.
//! - [ ] A bounded CI run over a rotating slice of the corpus.
//!
//! One more gap the calibration cannot see: a site whose offset lands inside a
//! string literal still parses, so [`injection_sites`] would keep it. Nothing
//! produces such an offset now that interpolation spans are rebased, but
//! checking a site against the token boundaries would close it by construction.

mod common;

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use wado_compiler::ast::{
    AstVisitor, Block, Function, Item, Module, Stmt, walk_block, walk_function, walk_item,
    walk_stmt,
};
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
            Excluded::BaselineCompileFailed { .. } => "baseline failed to compile",
            Excluded::BaselineUnhealthy { .. } => "baseline did not pass",
            Excluded::GuardRejected { .. } => "guard failed to compile",
            Excluded::GuardChangedOutput { .. } => "guard changed the output",
            Excluded::GuardCrashed { .. } => "guard crashed the compiler",
        }
    }

    fn detail(&self) -> String {
        match self {
            Excluded::MalformedData(d) | Excluded::FormatFailed(d) => d.clone(),
            Excluded::UnsupportedDataKey(k) => k.clone(),
            Excluded::Todo | Excluded::NoInjectionSite => String::new(),
            Excluded::BaselineCompileFailed { level, detail }
            | Excluded::BaselineUnhealthy { level, detail }
            | Excluded::GuardRejected { level, detail }
            | Excluded::GuardChangedOutput { level, detail }
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
struct Site {
    offset: usize,
    kind: &'static str,
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
}

impl SiteCollector {
    fn collect(module: &Module) -> Vec<Site> {
        let mut collector = Self {
            sites: Vec::new(),
            body_depth: 0,
        };
        for item in &module.items {
            collector.visit_item(item);
        }
        collector.sites.sort_unstable_by_key(|site| site.offset);
        collector.sites.dedup_by_key(|site| site.offset);
        collector.sites
    }
}

impl AstVisitor for SiteCollector {
    fn visit_item(&mut self, item: &Item) {
        // A `test` body is a function body in every way that matters here; the
        // rest reach their bodies through `visit_function`.
        let is_test = matches!(item, Item::Test(_));
        if is_test {
            self.body_depth += 1;
        }
        walk_item(self, item);
        if is_test {
            self.body_depth -= 1;
        }
    }

    fn visit_function(&mut self, func: &Function) {
        self.body_depth += 1;
        walk_function(self, func);
        self.body_depth -= 1;
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
        if self.body_depth > 0 {
            for stmt in &block.stmts {
                // A local item carries its attributes outside its span, so the
                // start offset would land between `#[...]` and the declaration.
                if matches!(stmt, Stmt::Item(_)) {
                    continue;
                }
                self.sites.push(Site {
                    offset: stmt.span().start,
                    kind: stmt_kind(stmt),
                });
            }
        }
        walk_block(self, block);
    }
}

/// Collect the sites of `source`, keeping only those a guard can actually be
/// written at.
///
/// A statement's span is not proof of a position: the parser re-lexes a
/// template interpolation on its own, so a node inside `${…}` carries an offset
/// relative to the fragment rather than to the file, and inserting there splices
/// a guard into unrelated text. Rather than enumerate which spans to distrust,
/// the collected set is checked against the parser — all at once, since a
/// fixture normally has nothing wrong with it, and site by site only when that
/// fails.
fn injection_sites(source: &str) -> Vec<Site> {
    let sites = SiteCollector::collect(&wado_compiler::parse(source).ast);
    if parses(&inject(source, &sites, "")) {
        return sites;
    }
    sites
        .into_iter()
        .filter(|site| parses(&inject(source, std::slice::from_ref(site), "")))
        .collect()
}

fn parses(source: &str) -> bool {
    wado_compiler::format(source).is_ok()
}

/// Insert `payload`, wrapped in a guard, at each of `sites`.
///
/// Offsets are consumed back to front so the earlier ones stay valid.
fn inject(source: &str, sites: &[Site], payload: &str) -> String {
    let text = guard(payload);
    let mut mutant = source.to_string();
    for site in sites.iter().rev() {
        mutant.insert_str(site.offset, &text);
    }
    mutant
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
    level: OptLevel,
    detail: String,
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

fn fixture_paths() -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let filter = std::env::var("WADO_EMI_FILTER").unwrap_or_default();
    let mut paths: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()))
        .map(|entry| entry.expect("directory entry").path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "wado"))
        .filter(|path| filter.is_empty() || path.to_string_lossy().contains(filter.as_str()))
        .collect();
    paths.sort();
    if let Ok(limit) = std::env::var("WADO_EMI_LIMIT") {
        let limit: usize = limit.parse().expect("WADO_EMI_LIMIT must be a number");
        paths.truncate(limit);
    }
    paths
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

/// Calibrate the fixture corpus: keep the fixtures an empty guard leaves alone.
///
/// `#[ignore]`d because it compiles and runs the whole corpus several times
/// over; run it on demand with `cargo test --test emi -- --ignored --nocapture`.
/// Silences the panic hook for as long as it is alive.
///
/// The campaign's workers are meant to panic: a mutant that crashes the
/// compiler is a finding, and [`evaluate`] catches it, so the default hook
/// would print a backtrace for every one. Restoring on drop is what keeps a
/// panic that escapes a worker — an unreadable fixture, a bad offset — from
/// leaving the rest of the test binary silent.
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

#[test]
#[ignore = "EMI campaign — minutes to hours over the full corpus"]
fn calibrate_corpus() {
    let paths = fixture_paths();
    let total = paths.len();
    assert!(total > 0, "no fixtures matched WADO_EMI_FILTER");

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
                    let name = path
                        .file_name()
                        .expect("fixture path has a file name")
                        .to_string_lossy()
                        .to_string();

                    let outcome = calibrate(path, &source);
                    let finished = done.fetch_add(1, Ordering::Relaxed) + 1;
                    if finished.is_multiple_of(50) {
                        eprintln!("[emi] {finished}/{total}");
                    }

                    let mut results = results.lock().expect("results lock");
                    match outcome {
                        Ok(eligible) => results.eligible.push(eligible),
                        Err(Excluded::GuardCrashed { level, detail }) => {
                            results.findings.push(Finding {
                                name,
                                level,
                                detail,
                            });
                        }
                        Err(excluded) => results.excluded.push((name, excluded)),
                    }
                }
            });
        }
    });

    let mut results = results.into_inner().expect("results lock");
    results.eligible.sort_by(|a, b| a.name.cmp(&b.name));
    results.excluded.sort_by(|a, b| a.0.cmp(&b.0));
    results.findings.sort_by(|a, b| a.name.cmp(&b.name));

    write_report(&results, total);

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

fn out_dir() -> PathBuf {
    std::env::var("WADO_EMI_OUT").map_or_else(
        |_| Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/emi"),
        PathBuf::from,
    )
}

fn write_report(results: &Results, total: usize) {
    let dir = out_dir();
    std::fs::create_dir_all(&dir)
        .unwrap_or_else(|e| panic!("cannot create {}: {e}", dir.display()));

    let mut corpus = String::new();
    for eligible in &results.eligible {
        corpus.push_str(&format!("{} {}\n", eligible.name, eligible.sites));
    }
    std::fs::write(dir.join("corpus.txt"), &corpus).expect("cannot write corpus.txt");

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
                "{} [{}] {}\n",
                finding.name,
                common::opt_level_name(finding.level),
                finding.detail
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

    let path = dir.join("calibration.txt");
    std::fs::write(&path, &report).expect("cannot write calibration.txt");
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
