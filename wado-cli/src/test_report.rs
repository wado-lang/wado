//! Output-format abstraction for `wado test`.
//!
//! The pipeline (`test.rs`) drives compile/load/execute purely on data; it
//! reports progress through a [`TestReporter`] rather than printing
//! directly, so different `--format` values can render the same event
//! stream differently (a human-readable log, a compact tailable digest, a
//! TAP document, ...).
//!
//! Events fire live as the pipeline discovers them (per file compiled,
//! per file loaded, per test executed). Totals used for the exit code are
//! always computed by the pipeline itself, independent of which reporter
//! is active — a reporter only decides how to *display* the run, never
//! what counts as success or failure.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::Notify;
use wado_compiler::hashmap::IndexMap;

use crate::test::{
    self, CompileFailure, LoadFailure, PackageRun, PackageTotals, TestOutcome, TestResult,
    TodoCompileError,
};

/// How often the compact reporter's heartbeat re-prints progress while
/// otherwise quiet. A failure/resolved-TODO notice resets this window
/// (see `CompactState::notify`) so the next heartbeat doesn't immediately
/// repeat what was just reported.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);

/// Outcome of compiling one file, as seen by a reporter.
pub(crate) enum CompileEvent {
    Ok,
    /// A `#![TODO]` module whose compile error fired as expected.
    TodoModule,
    /// `detail` carries a message for the rare host-side failures (worker
    /// panic, blocking-task join error) that have no per-file compiler
    /// diagnostic of their own.
    Failed {
        detail: Option<String>,
    },
}

/// Outcome of loading one compiled file into wasmtime, as seen by a reporter.
pub(crate) enum LoadEvent {
    Ok { test_count: usize },
    Failed { detail: Option<String> },
}

/// Everything a reporter needs to render one package's final result.
/// `totals` is already-computed and authoritative (drives the exit code);
/// the rest is the raw material a batch-style reporter needs to lay out
/// per-file/per-test detail.
pub(crate) struct PackageDoneArgs<'a> {
    pub(crate) pkg_run: &'a PackageRun,
    pub(crate) totals: &'a PackageTotals,
    pub(crate) test_results: &'a [TestResult],
    pub(crate) todo_compile_errors: &'a [TodoCompileError],
    pub(crate) compile_failures: &'a [CompileFailure],
    pub(crate) load_failures: &'a [LoadFailure],
    pub(crate) no_run: bool,
}

/// Sink for the pipeline's progress events. One instance is constructed
/// per `wado test` invocation and shared (via `Arc`) across every package
/// run and every pipeline worker task.
pub(crate) trait TestReporter: Send + Sync {
    fn on_package_start(&self, pkg_run: &PackageRun, show_banner: bool);
    fn on_compile(&self, path: &str, event: CompileEvent, duration: Duration);
    fn on_load(&self, path: &str, event: LoadEvent, duration: Duration);
    fn on_test_result(&self, result: &TestResult);
    fn on_package_done(&self, args: PackageDoneArgs);
    fn on_run_done(&self, grand: &PackageTotals, multi_pkg: bool, wall: Duration);
}

/// Reproduces `wado test`'s original output: per-file compile/load lines
/// streamed live, per-test results and summary sections printed as one
/// batch per package once its pipeline drains.
pub(crate) struct VerboseReporter {
    overall_start: Instant,
}

impl VerboseReporter {
    pub(crate) fn new(overall_start: Instant) -> Self {
        Self { overall_start }
    }

    fn elapsed(&self) -> String {
        test::format_duration(self.overall_start.elapsed())
    }
}

impl TestReporter for VerboseReporter {
    fn on_package_start(&self, pkg_run: &PackageRun, show_banner: bool) {
        if show_banner {
            println!();
            println!("=== package: {} ===", pkg_run.label);
        }
    }

    fn on_compile(&self, path: &str, event: CompileEvent, duration: Duration) {
        let elapsed = self.elapsed();
        let dur = test::format_duration(duration);
        match event {
            CompileEvent::Ok => println!("[{elapsed}] Compiled {path} ({dur})"),
            CompileEvent::TodoModule => {
                println!(
                    "[{elapsed}] Compiled {path} (TODO module, compile error expected, {dur})"
                );
            }
            CompileEvent::Failed { detail: None } => {
                eprintln!("[{elapsed}] FAILED to compile {path} ({dur})");
            }
            CompileEvent::Failed { detail: Some(msg) } => {
                eprintln!("[{elapsed}] FAILED to compile {path} ({dur}): {msg}");
            }
        }
    }

    fn on_load(&self, path: &str, event: LoadEvent, duration: Duration) {
        let elapsed = self.elapsed();
        let dur = test::format_duration(duration);
        match event {
            LoadEvent::Ok { .. } => println!("[{elapsed}] Loaded {path} ({dur})"),
            LoadEvent::Failed { detail: None } => {
                eprintln!("[{elapsed}] FAILED to load {path} ({dur})");
            }
            LoadEvent::Failed { detail: Some(msg) } => {
                eprintln!("[{elapsed}] FAILED to load {path} ({dur}): {msg}");
            }
        }
    }

    fn on_test_result(&self, _result: &TestResult) {
        // Verbose prints test results in one batch per package, in
        // `on_package_done` — see the module doc for why.
    }

    fn on_package_done(&self, args: PackageDoneArgs) {
        if args.no_run {
            let todo_entries: Vec<test::TodoEntry> = args
                .todo_compile_errors
                .iter()
                .map(|e| test::TodoEntry {
                    file_path: e.path.clone(),
                    display_name: "#![TODO] module".to_string(),
                    resolved: false,
                })
                .collect();
            test::print_compile_failures_section(args.compile_failures);
            test::print_todo_section(&todo_entries, 0);
            println!();
            test::print_three_axis(args.totals, None);
            return;
        }

        let mut results_by_file: wado_compiler::hashmap::IndexMap<&str, Vec<&TestResult>> =
            wado_compiler::hashmap::IndexMap::default();
        for result in args.test_results {
            results_by_file
                .entry(result.file_path.as_str())
                .or_default()
                .push(result);
        }
        let todo_error_by_path: wado_compiler::hashmap::IndexMap<&str, &TodoCompileError> = args
            .todo_compile_errors
            .iter()
            .map(|e| (e.path.as_str(), e))
            .collect();

        let report =
            test::display_test_results(&args.pkg_run.paths, &results_by_file, &todo_error_by_path);
        test::print_failure_section(&report.fail_entries);
        test::print_todo_section(&report.todo_entries, report.todo_resolved);
        test::print_compile_failures_section(args.compile_failures);
        test::print_load_failures_section(args.load_failures);

        println!();
        test::print_three_axis(args.totals, None);
    }

    fn on_run_done(&self, grand: &PackageTotals, multi_pkg: bool, wall: Duration) {
        let total_dur = test::format_duration(wall);
        if multi_pkg {
            println!();
            println!("=== aggregate ===");
            test::print_three_axis(grand, Some(&total_dur));
        } else {
            println!("(wall: {total_dur})");
        }
    }
}

/// Shared, atomically-updated counters the heartbeat ticker reads and
/// every trait method writes to. A file counts as "done" the moment its
/// outcome is fully known: immediately on a compile/load failure or a
/// `#![TODO]` compile error, immediately on load with zero test blocks
/// (a SKIP), or once its last test result arrives (tracked via
/// `pending_tests`).
struct CompactState {
    overall_start: Instant,
    total_files: usize,
    files_done: AtomicUsize,
    tests_seen: AtomicU32,
    tests_failed: AtomicU32,
    todo_pending: AtomicU32,
    todo_resolved: AtomicU32,
    skip_files: AtomicUsize,
    pending_tests: Mutex<IndexMap<String, usize>>,
    /// Signalled on every immediate failure/resolved-TODO notice so the
    /// heartbeat loop can restart its wait — avoids printing a near-
    /// duplicate heartbeat right after an event that already reported.
    notify: Notify,
    stop: AtomicBool,
}

impl CompactState {
    fn mark_file_done(&self) {
        self.files_done.fetch_add(1, Ordering::Relaxed);
    }
}

fn print_heartbeat(state: &CompactState) {
    let elapsed = state.overall_start.elapsed();
    let done = state.files_done.load(Ordering::Relaxed);
    let total = state.total_files;
    let tests = state.tests_seen.load(Ordering::Relaxed);
    let failed = state.tests_failed.load(Ordering::Relaxed);
    let todo = state.todo_pending.load(Ordering::Relaxed);
    let resolved = state.todo_resolved.load(Ordering::Relaxed);
    let skip = state.skip_files.load(Ordering::Relaxed);

    let pct = done
        .checked_mul(100)
        .and_then(|p| p.checked_div(total))
        .map_or_else(String::new, |p| format!(" ({p}%)"));
    // File-count-based ETA only: total test count isn't known upfront
    // (a file's test blocks are discovered when it loads), so files are
    // the only axis with a stable denominator to extrapolate from.
    let eta = if done > 0 && done < total {
        let remaining = elapsed.as_secs_f64() / done as f64 * (total - done) as f64;
        format!(" · ETA ~{}s", remaining.round() as u64)
    } else {
        String::new()
    };

    let mut counts = format!("{tests} tests, {failed} failed");
    if todo > 0 {
        counts.push_str(&format!(", {todo} todo"));
    }
    if resolved > 0 {
        counts.push_str(&format!(", {resolved} resolved"));
    }
    if skip > 0 {
        counts.push_str(&format!(", {skip} skip"));
    }

    println!(
        "[{}] {done}/{total} files{pct} · {counts}{eta}",
        test::format_duration(elapsed)
    );
}

/// Tailable default: immediate one-line notices for anything that needs
/// attention (failures, resolved TODOs), a periodic heartbeat digest
/// otherwise, and a final three-axis summary.
pub(crate) struct CompactReporter {
    state: Arc<CompactState>,
}

impl CompactReporter {
    pub(crate) fn new(overall_start: Instant, total_files: usize) -> Self {
        let state = Arc::new(CompactState {
            overall_start,
            total_files,
            files_done: AtomicUsize::new(0),
            tests_seen: AtomicU32::new(0),
            tests_failed: AtomicU32::new(0),
            todo_pending: AtomicU32::new(0),
            todo_resolved: AtomicU32::new(0),
            skip_files: AtomicUsize::new(0),
            pending_tests: Mutex::new(IndexMap::default()),
            notify: Notify::new(),
            stop: AtomicBool::new(false),
        });

        print_heartbeat(&state);

        let ticker_state = Arc::clone(&state);
        tokio::spawn(async move {
            loop {
                // Wait for the heartbeat interval, but bail out early (and
                // restart the wait) if a failure/resolved-TODO notice fires
                // in the meantime — avoids an immediate near-duplicate line.
                let notified =
                    tokio::time::timeout(HEARTBEAT_INTERVAL, ticker_state.notify.notified()).await;
                if ticker_state.stop.load(Ordering::Relaxed) {
                    break;
                }
                if notified.is_err() {
                    print_heartbeat(&ticker_state);
                }
            }
        });

        Self { state }
    }

    /// Print an immediate one-line notice and wake the heartbeat loop so
    /// it doesn't immediately repeat the same information.
    fn announce(&self, line: &str) {
        println!("{line}");
        self.state.notify.notify_one();
    }

    fn finish_file_if_done(&self, path: &str) {
        let mut pending = self
            .state
            .pending_tests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(remaining) = pending.get_mut(path) {
            *remaining -= 1;
            if *remaining == 0 {
                pending.swap_remove(path);
                drop(pending);
                self.state.mark_file_done();
            }
        }
    }
}

impl TestReporter for CompactReporter {
    fn on_package_start(&self, _pkg_run: &PackageRun, _show_banner: bool) {
        // Compact treats the whole invocation as one continuous run — no
        // per-package banners, consistent with the single running total
        // the heartbeat reports.
    }

    fn on_compile(&self, path: &str, event: CompileEvent, _duration: Duration) {
        match event {
            CompileEvent::Ok => {}
            CompileEvent::TodoModule => self.state.mark_file_done(),
            CompileEvent::Failed { detail } => {
                self.state.mark_file_done();
                let suffix = detail.map(|d| format!(": {d}")).unwrap_or_default();
                self.announce(&format!("not ok  {path} (compile failed){suffix}"));
            }
        }
    }

    fn on_load(&self, path: &str, event: LoadEvent, _duration: Duration) {
        match event {
            LoadEvent::Ok { test_count: 0 } => {
                self.state.mark_file_done();
                self.state.skip_files.fetch_add(1, Ordering::Relaxed);
            }
            LoadEvent::Ok { test_count } => {
                let mut pending = self
                    .state
                    .pending_tests
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                pending.insert(path.to_string(), test_count);
            }
            LoadEvent::Failed { detail } => {
                self.state.mark_file_done();
                let suffix = detail.map(|d| format!(": {d}")).unwrap_or_default();
                self.announce(&format!("not ok  {path} (load failed){suffix}"));
            }
        }
    }

    fn on_test_result(&self, result: &TestResult) {
        self.state.tests_seen.fetch_add(1, Ordering::Relaxed);
        match result.outcome {
            TestOutcome::Pass => {}
            TestOutcome::Fail => {
                self.state.tests_failed.fetch_add(1, Ordering::Relaxed);
                let dur = test::format_duration(result.duration);
                let detail = result
                    .error
                    .as_ref()
                    .map(|e| format!("\n  {e}"))
                    .unwrap_or_default();
                self.announce(&format!(
                    "not ok  {} :: {} ({dur}){detail}",
                    result.file_path, result.display_name
                ));
            }
            TestOutcome::TodoPending => {
                self.state.todo_pending.fetch_add(1, Ordering::Relaxed);
            }
            TestOutcome::TodoResolved => {
                self.state.todo_resolved.fetch_add(1, Ordering::Relaxed);
                self.announce(&format!(
                    "resolved  {} :: {} — remove the #[TODO] attribute",
                    result.file_path, result.display_name
                ));
            }
        }
        self.finish_file_if_done(&result.file_path);
    }

    fn on_package_done(&self, _args: PackageDoneArgs) {
        // Everything relevant already streamed live via the other hooks;
        // the authoritative totals are rendered once in `on_run_done`.
    }

    fn on_run_done(&self, grand: &PackageTotals, _multi_pkg: bool, wall: Duration) {
        self.state.stop.store(true, Ordering::Relaxed);
        self.state.notify.notify_one();
        println!();
        test::print_three_axis(grand, Some(&test::format_duration(wall)));
    }
}

/// One YAML diagnostic block (TAP13/14 `---` … `...`), rendered as a
/// literal block scalar so multi-line error text (assertion diagnostics,
/// wasm backtraces) needs no escaping. `indent` is the Test Point's own
/// indentation — the block markers sit at `indent` + 2 spaces, matching
/// the spec's "YAML indented 2 spaces past its Test Point" rule.
fn yaml_block(indent: &str, message: &str) -> Vec<String> {
    let mut lines = vec![format!("{indent}  ---"), format!("{indent}  message: |")];
    for line in message.lines() {
        lines.push(format!("{indent}    {line}"));
    }
    lines.push(format!("{indent}  ..."));
    lines
}

/// One file's subtest bookkeeping: its declared test count (known once it
/// loads, before any test runs), how many results have arrived, whether
/// any counted as a failure, and its body lines — printed immediately if
/// this file is the active subtest, buffered otherwise.
struct TapFileState {
    test_count: usize,
    seen: usize,
    any_failed: bool,
    opened: bool,
    lines: Vec<String>,
}

/// Serializes concurrent files into one valid nested TAP document.
///
/// TAP's subtest nesting is indentation-based, so only one subtest can be
/// "open" in the output stream at a time — a top-level Test Point from a
/// different, concurrently-finishing file can't be interleaved into the
/// middle of another file's indented block without corrupting the
/// structure. `active` names the file currently allowed to print; every
/// other file's lines buffer in `entries`/`standalone` until their turn.
/// This only serializes *printing* — compilation/loading/execution keep
/// running fully concurrently underneath.
struct TapDoc {
    /// Subtest-bearing files, insertion-ordered. Order of *printing*
    /// doesn't have to match registration order (TAP has no such
    /// requirement) — `advance` just picks the first unopened entry.
    entries: IndexMap<String, TapFileState>,
    /// Fully-formed single-shot Test Points (compile/load failure, skip,
    /// TODO-module) ready to print as soon as it's their turn.
    standalone: VecDeque<String>,
    active: Option<String>,
}

impl TapDoc {
    fn new() -> Self {
        Self {
            entries: IndexMap::default(),
            standalone: VecDeque::new(),
            active: None,
        }
    }

    /// Make as much progress as possible: flush queued standalone points,
    /// then open the next unopened subtest, repeating while the document
    /// is idle. Stops as soon as something is active (waiting on more
    /// test results) or there's nothing left ready to print.
    fn advance(&mut self) {
        loop {
            if self.active.is_some() {
                return;
            }
            if let Some(block) = self.standalone.pop_front() {
                println!("{block}");
                continue;
            }
            let Some(path) = self
                .entries
                .iter()
                .find(|(_, e)| !e.opened)
                .map(|(k, _)| k.clone())
            else {
                return;
            };
            let entry = self.entries.get_mut(&path).expect("path was just found");
            entry.opened = true;
            println!("# Subtest: {path}");
            println!("    1..{}", entry.test_count);
            for line in &entry.lines {
                println!("{line}");
            }
            let done = entry.seen >= entry.test_count;
            self.active = Some(path);
            if done {
                self.close_active();
            }
        }
    }

    fn close_active(&mut self) {
        let Some(path) = self.active.take() else {
            return;
        };
        let entry = self
            .entries
            .swap_remove(&path)
            .expect("active entry exists");
        let point = if entry.any_failed { "not ok" } else { "ok" };
        println!("{point} - {path}");
    }

    fn register_subtest(&mut self, path: String, test_count: usize) {
        self.entries.insert(
            path,
            TapFileState {
                test_count,
                seen: 0,
                any_failed: false,
                opened: false,
                lines: Vec::new(),
            },
        );
        self.advance();
    }

    fn register_standalone(&mut self, block: String) {
        self.standalone.push_back(block);
        self.advance();
    }

    fn record_result(&mut self, path: &str, failed: bool, lines: Vec<String>) {
        let is_active = self.active.as_deref() == Some(path);
        let Some(entry) = self.entries.get_mut(path) else {
            return;
        };
        entry.seen += 1;
        if failed {
            entry.any_failed = true;
        }
        let done = entry.seen >= entry.test_count;
        if is_active {
            for line in &lines {
                println!("{line}");
            }
        } else {
            entry.lines.extend(lines);
        }
        if is_active && done {
            self.close_active();
            self.advance();
        }
    }
}

/// TAP14 output: `TAP version 14` and a leading `1..{file count}` plan
/// (the file count is known upfront from discovery, before anything
/// compiles), one top-level Test Point per file, and — for files with
/// `test` blocks — a `# Subtest:` block nesting each test's own Test
/// Point. `#[TODO]` maps onto TAP's own `TODO` directive (the feature it
/// was modeled on); a TODO test that unexpectedly passes is reported
/// honestly as `ok … # TODO resolved` rather than forced to `not ok` —
/// whether that counts as a failure is `wado test`'s own policy (see
/// `on_run_done`/the exit code in `test::run`), not something the raw TAP
/// text should misrepresent. Compile/load progress and the final summary
/// are `#` comments, which TAP consumers ignore but a human or an agent
/// tailing the stream can still read.
pub(crate) struct TapReporter {
    overall_start: Instant,
    doc: Mutex<TapDoc>,
}

impl TapReporter {
    pub(crate) fn new(overall_start: Instant, total_files: usize) -> Self {
        println!("TAP version 14");
        println!("1..{total_files}");
        Self {
            overall_start,
            doc: Mutex::new(TapDoc::new()),
        }
    }

    fn comment(&self, text: &str) {
        println!(
            "# [{}] {text}",
            test::format_duration(self.overall_start.elapsed())
        );
    }

    fn doc(&self) -> std::sync::MutexGuard<'_, TapDoc> {
        self.doc
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl TestReporter for TapReporter {
    fn on_package_start(&self, pkg_run: &PackageRun, show_banner: bool) {
        if show_banner {
            self.comment(&format!("package: {}", pkg_run.label));
        }
    }

    fn on_compile(&self, path: &str, event: CompileEvent, duration: Duration) {
        let dur = test::format_duration(duration);
        match event {
            CompileEvent::Ok => self.comment(&format!("Compiled {path} ({dur})")),
            CompileEvent::TodoModule => {
                self.comment(&format!(
                    "Compiled {path} (TODO module, compile error expected, {dur})"
                ));
                self.doc().register_standalone(format!(
                    "ok - {path} # TODO module compile error expected"
                ));
            }
            CompileEvent::Failed { detail } => {
                self.comment(&format!("FAILED to compile {path} ({dur})"));
                let mut block = vec![format!("not ok - {path} (compile failed)")];
                if let Some(msg) = detail {
                    block.extend(yaml_block("", &msg));
                }
                self.doc().register_standalone(block.join("\n"));
            }
        }
    }

    fn on_load(&self, path: &str, event: LoadEvent, duration: Duration) {
        let dur = test::format_duration(duration);
        match event {
            LoadEvent::Ok { test_count: 0 } => {
                self.comment(&format!("Loaded {path} ({dur})"));
                self.doc()
                    .register_standalone(format!("ok - {path} # SKIP no test blocks"));
            }
            LoadEvent::Ok { test_count } => {
                self.comment(&format!("Loaded {path} ({dur})"));
                self.doc().register_subtest(path.to_string(), test_count);
            }
            LoadEvent::Failed { detail } => {
                self.comment(&format!("FAILED to load {path} ({dur})"));
                let mut block = vec![format!("not ok - {path} (load failed)")];
                if let Some(msg) = detail {
                    block.extend(yaml_block("", &msg));
                }
                self.doc().register_standalone(block.join("\n"));
            }
        }
    }

    fn on_test_result(&self, result: &TestResult) {
        let dur = test::format_duration(result.duration);
        let name = &result.display_name;
        let (mut lines, failed) = match result.outcome {
            TestOutcome::Pass => (vec![format!("ok - {name} ({dur})")], false),
            TestOutcome::Fail => {
                let mut l = vec![format!("not ok - {name} ({dur})")];
                if let Some(ref msg) = result.error {
                    l.extend(yaml_block("", msg));
                }
                (l, true)
            }
            TestOutcome::TodoPending => (vec![format!("not ok - {name} ({dur}) # TODO")], false),
            TestOutcome::TodoResolved => (
                vec![format!(
                    "ok - {name} ({dur}) # TODO resolved — remove the #[TODO] attribute"
                )],
                false,
            ),
        };
        // One subtest nesting level: 4-space indent on every body line,
        // diagnostic blocks included.
        for line in &mut lines {
            *line = format!("    {line}");
        }
        self.doc().record_result(&result.file_path, failed, lines);
    }

    fn on_package_done(&self, _args: PackageDoneArgs) {
        // Everything streams live through the other hooks; there's no
        // per-package concept in a single TAP document.
    }

    fn on_run_done(&self, grand: &PackageTotals, _multi_pkg: bool, wall: Duration) {
        self.comment(&format!(
            "compile: {} ok, {} failed",
            grand.compile_ok, grand.compile_failed
        ));
        if grand.load_ok + grand.load_failed > 0 {
            self.comment(&format!(
                "load: {} ok, {} failed",
                grand.load_ok, grand.load_failed
            ));
        }
        if grand.skip_files > 0 {
            self.comment(&format!(
                "skip: {} files (no test blocks)",
                grand.skip_files
            ));
        }
        self.comment(&format!(
            "test: {} passed, {} failed",
            grand.test_passed, grand.test_failed
        ));
        if grand.todo_pending + grand.todo_resolved > 0 {
            self.comment(&format!(
                "todo: {} pending, {} resolved",
                grand.todo_pending, grand.todo_resolved
            ));
        }
        self.comment(&format!("wall: {}", test::format_duration(wall)));
    }
}
