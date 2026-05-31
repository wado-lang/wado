use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use wado_compiler::OptLevel;

use crate::compiler_host::FilesystemCompilerHost;
use crate::data_section::{extract_world_from_data_section, should_skip_file};
use crate::template::Template;

const COMPILER_STACK_SIZE: usize = 16 * 1024 * 1024;

/// Default worker count: one per logical CPU. Each worker owns a 16 MB stack
/// and runs the full compiler plus wasmtime. On constrained sandboxes this has
/// empirically caused SIGSEGV from resource exhaustion; set `WADO_GOLDEN_WORKERS`
/// to cap the parallelism in that case.
fn default_num_workers() -> usize {
    if let Some(n) = std::env::var("WADO_GOLDEN_WORKERS")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|&n| n > 0)
    {
        return n;
    }
    std::thread::available_parallelism()
        .map(std::num::NonZero::get)
        .unwrap_or(4)
}

/// Per-worker slot tracking which file is currently being compiled.
static ACTIVE_FILES: std::sync::LazyLock<Vec<std::sync::Mutex<Option<(String, Instant)>>>> =
    std::sync::LazyLock::new(|| {
        let n = default_num_workers();
        (0..n).map(|_| std::sync::Mutex::new(None)).collect()
    });

/// Install signal handlers (SIGINT, SIGTERM, SIGALRM) that dump active files before exiting.
#[cfg(unix)]
fn install_signal_handlers() {
    unsafe {
        for sig in [libc::SIGINT, libc::SIGTERM, libc::SIGALRM] {
            libc::signal(sig, dump_active_files_and_exit as *const () as usize);
        }
    }
}

#[cfg(unix)]
extern "C" fn dump_active_files_and_exit(sig: libc::c_int) {
    // Signal handler — keep it async-signal-safe as much as possible.
    // We use write(2) directly for the header, but for the file list we
    // accept the small risk of using Mutex (workers are likely stuck, not holding locks).
    let header = b"\n=== TIMEOUT/SIGNAL: active compilations ===\n";
    unsafe {
        libc::write(2, header.as_ptr().cast(), header.len());
    }
    for (i, slot) in ACTIVE_FILES.iter().enumerate() {
        if let Ok(guard) = slot.try_lock()
            && let Some((ref path, start)) = *guard
        {
            let elapsed = start.elapsed().as_secs();
            let msg = format!("  worker {i}: {path} ({elapsed}s)\n");
            unsafe {
                libc::write(2, msg.as_ptr().cast(), msg.len());
            }
        }
    }
    let footer = b"===========================================\n";
    unsafe {
        libc::write(2, footer.as_ptr().cast(), footer.len());
        libc::_exit(128 + sig);
    }
}

#[cfg(unix)]
const ALT_STACK_SIZE: usize = 8 * 1024 * 1024;

#[cfg(unix)]
fn install_alt_stack() {
    let mem = vec![0u8; ALT_STACK_SIZE].into_boxed_slice();
    let ptr = Box::into_raw(mem);
    unsafe {
        let ss = libc::stack_t {
            ss_sp: ptr.cast::<libc::c_void>(),
            ss_flags: 0,
            ss_size: ALT_STACK_SIZE,
        };
        libc::sigaltstack(&raw const ss, std::ptr::null_mut());
    }
}

struct ReadItem {
    name: String,
    source: String,
    input_path: String,
}

/// One fixture compiled into every requested emit. `outputs` carries one entry
/// per `--emit`, in the same order as the emit specs, so the collector can map
/// each result back to its output path and track which names were written.
struct WriteBatch {
    name: String,
    outputs: Vec<EmitOutput>,
    compile_time: Duration,
}

struct EmitOutput {
    emit_index: usize,
    output_path: PathBuf,
    content: Result<String, String>,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub enum Phase {
    Wir,
    Nir,
    NirLowered,
    Wat,
}

/// A single `--emit <phase>:<out-template>` request.
pub struct Emit {
    pub phase: Phase,
    pub out_template: String,
}

/// Golden files that exist on disk but were not (re)written in this run.
///
/// This is the heart of stale pruning: the set of golden files matching an
/// emit's `prefix{name}suffix` pattern must end up being exactly the set we
/// wrote. Any pre-existing file whose `{name}` we did not write — because its
/// fixture was deleted, marked `compile_error`/`TODO`, or produced empty output
/// under `--skip-empty` — is stale and returned here for deletion.
fn collect_stale(existing: Vec<(String, PathBuf)>, written: &HashSet<String>) -> Vec<PathBuf> {
    existing
        .into_iter()
        .filter(|(name, _)| !written.contains(name))
        .map(|(_, path)| path)
        .collect()
}

pub fn run_pipeline(in_template: &str, emits: &[Emit], opt_level: OptLevel, skip_empty: bool) {
    assert!(!emits.is_empty(), "at least one --emit is required");

    let start = Instant::now();

    #[cfg(unix)]
    install_signal_handlers();

    let num_workers = default_num_workers();

    // Force ACTIVE_FILES initialization with correct worker count
    let _ = &*ACTIVE_FILES;

    let in_tmpl = Template::parse(in_template);

    let files = in_tmpl.discover();
    let total = files.len();

    // Pre-filter fixtures that are not meant to compile (compile_error / TODO).
    // We simply do not compile them; their stale golden files, if any, are
    // removed by the end-of-run prune like any other unwritten name.
    let mut read_items: Vec<ReadItem> = Vec::new();
    let mut prefiltered = 0u32;

    for (name, path) in &files {
        let source = fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("Failed to read {}: {e}", path.display()));
        if should_skip_file(&source) {
            prefiltered += 1;
            continue;
        }
        read_items.push(ReadItem {
            name: name.clone(),
            source,
            input_path: path.to_string_lossy().into_owned(),
        });
    }

    let compile_count = read_items.len();

    // Distinct phases actually requested, so each fixture is compiled at most
    // once per phase even when several emits share a phase (e.g. `.nir` and
    // `.optimize` both dump NIR at -O2).
    let mut distinct_phases: Vec<Phase> = Vec::new();
    for emit in emits {
        if !distinct_phases.contains(&emit.phase) {
            distinct_phases.push(emit.phase);
        }
    }

    // Output templates shared with every worker, parsed once and reused for
    // both per-fixture path building and the end-of-run stale prune.
    let emit_templates: std::sync::Arc<Vec<(Phase, Template)>> = std::sync::Arc::new(
        emits
            .iter()
            .map(|e| (e.phase, Template::parse(&e.out_template)))
            .collect(),
    );
    let distinct_phases = std::sync::Arc::new(distinct_phases);

    // Channels
    let (read_tx, read_rx) = mpsc::sync_channel::<ReadItem>(2 * num_workers);
    let (write_tx, write_rx) = mpsc::sync_channel::<WriteBatch>(num_workers);

    // Reader thread: sends pre-read items
    let reader = std::thread::spawn(move || {
        for item in read_items {
            if read_tx.send(item).is_err() {
                break;
            }
        }
        // drop read_tx to signal EOF
    });

    // Shared receiver for work-stealing among dump workers
    let read_rx = std::sync::Arc::new(std::sync::Mutex::new(read_rx));

    // Dump worker threads
    let mut workers = Vec::with_capacity(num_workers);
    for worker_id in 0..num_workers {
        let rx = read_rx.clone();
        let tx = write_tx.clone();
        let emit_templates = emit_templates.clone();
        let distinct_phases = distinct_phases.clone();

        workers.push(
            std::thread::Builder::new()
                .stack_size(COMPILER_STACK_SIZE)
                .spawn(move || {
                    #[cfg(unix)]
                    install_alt_stack();

                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .expect("failed to build per-worker tokio runtime");

                    loop {
                        let item = {
                            let guard =
                                rx.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                            match guard.recv() {
                                Ok(item) => item,
                                Err(_) => break, // channel closed
                            }
                        };

                        let input_path = item.input_path.clone();

                        // Track active compilation for signal handler
                        let t0 = Instant::now();
                        if let Some(slot) = ACTIVE_FILES.get(worker_id) {
                            *slot.lock().unwrap() = Some((input_path.clone(), t0));
                        }

                        let rendered: HashMap<Phase, Result<String, String>> =
                            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                rt.block_on(render_phases(
                                    &item.source,
                                    &item.input_path,
                                    &distinct_phases,
                                    opt_level,
                                ))
                            }))
                            .unwrap_or_else(|panic_val| {
                                let msg = match panic_val.downcast_ref::<String>() {
                                    Some(s) => s.clone(),
                                    None => match panic_val.downcast_ref::<&str>() {
                                        Some(s) => (*s).to_string(),
                                        None => "unknown panic".to_string(),
                                    },
                                };
                                distinct_phases
                                    .iter()
                                    .map(|p| (*p, Err(format!("panic in {input_path}: {msg}"))))
                                    .collect()
                            });
                        let compile_time = t0.elapsed();

                        // Clear active file
                        if let Some(slot) = ACTIVE_FILES.get(worker_id) {
                            *slot.lock().unwrap() = None;
                        }

                        let outputs = emit_templates
                            .iter()
                            .enumerate()
                            .map(|(emit_index, (phase, tmpl))| {
                                let output_path = tmpl.output_path(&item.name);
                                let content = rendered.get(phase).cloned().unwrap_or_else(|| {
                                    Err("internal error: phase not rendered".to_string())
                                });
                                EmitOutput {
                                    emit_index,
                                    output_path,
                                    content,
                                }
                            })
                            .collect();

                        if tx
                            .send(WriteBatch {
                                name: item.name.clone(),
                                outputs,
                                compile_time,
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                })
                .expect("failed to spawn dump worker"),
        );
    }
    // Drop the original write_tx so writer sees EOF when all workers finish
    drop(write_tx);

    // Collect results, writing each output in place. No file is removed here:
    // every golden file is overwritten where it already lives, so there is no
    // moment at which the golden set is empty or partially deleted.
    let mut written: Vec<HashSet<String>> = vec![HashSet::new(); emits.len()];
    let mut generated = 0u32;
    let mut skipped = 0u32;
    let mut total_compile_time = Duration::ZERO;
    let mut max_compile_time = Duration::ZERO;
    let mut max_compile_file = String::new();

    for batch in write_rx {
        total_compile_time += batch.compile_time;
        if batch.compile_time > max_compile_time {
            max_compile_time = batch.compile_time;
            max_compile_file.clone_from(&batch.name);
        }

        for output in batch.outputs {
            let EmitOutput {
                emit_index,
                output_path,
                content,
            } = output;
            let display = output_path.display();
            match content {
                Ok(text) if text.is_empty() => {
                    if skip_empty {
                        eprintln!("  Skipped {display} (empty output)");
                        skipped += 1;
                        continue;
                    }
                    panic!(
                        "Golden fixture generation failed: Empty output for {display}\n\
                         If this test is expected to fail at compile time, add \
                         \"compile_error\" or \"TODO\" to its __DATA__ section."
                    );
                }
                Ok(text) => {
                    if let Some(parent) = output_path.parent()
                        && !parent.as_os_str().is_empty()
                    {
                        let _ = fs::create_dir_all(parent);
                    }
                    fs::write(&output_path, &text)
                        .unwrap_or_else(|e| panic!("Failed to write {display}: {e}"));
                    eprintln!("  Generated {display}");
                    written[emit_index].insert(batch.name.clone());
                    generated += 1;
                }
                Err(e) => {
                    if skip_empty {
                        eprintln!("  Skipped {display} ({e})");
                        skipped += 1;
                        continue;
                    }
                    panic!(
                        "Golden fixture generation failed: {e}\n\
                         If this test is expected to fail at compile time, add \
                         \"compile_error\" or \"TODO\" to its __DATA__ section."
                    );
                }
            }
        }
    }

    reader.join().expect("reader thread panicked");
    for w in workers {
        w.join().expect("dump worker panicked");
    }

    // Prune stale golden files: anything matching an emit's pattern whose
    // `{name}` we did not just write. This runs only after every output has
    // been regenerated, so the workspace never shows a mass deletion mid-run.
    let mut pruned = 0u32;
    for ((_, tmpl), written) in emit_templates.iter().zip(written.iter()) {
        for path in collect_stale(tmpl.discover(), written) {
            match fs::remove_file(&path) {
                Ok(()) => {
                    eprintln!("  Pruned {}", path.display());
                    pruned += 1;
                }
                Err(e) => eprintln!("  Warning: failed to prune {}: {e}", path.display()),
            }
        }
    }

    print_stats(&PipelineStats {
        generated,
        skipped,
        prefiltered,
        pruned,
        total,
        compile_count,
        num_workers,
        elapsed: start.elapsed(),
        total_compile_time,
        max_compile_time,
        max_compile_file,
    });
}

struct PipelineStats {
    generated: u32,
    skipped: u32,
    prefiltered: u32,
    pruned: u32,
    total: usize,
    compile_count: usize,
    num_workers: usize,
    elapsed: Duration,
    total_compile_time: Duration,
    max_compile_time: Duration,
    max_compile_file: String,
}

fn print_stats(stats: &PipelineStats) {
    let avg_compile = if stats.compile_count > 0 {
        stats.total_compile_time / stats.compile_count as u32
    } else {
        Duration::ZERO
    };
    eprintln!();
    eprintln!(
        "  Fixtures:     {} total, {} compiled, {} pre-filtered",
        stats.total, stats.compile_count, stats.prefiltered
    );
    eprintln!(
        "  Outputs:      {} generated, {} skipped, {} pruned",
        stats.generated, stats.skipped, stats.pruned
    );
    eprintln!("  Workers:      {}", stats.num_workers);
    eprintln!("  Wall time:    {:.2}s", stats.elapsed.as_secs_f64());
    eprintln!(
        "  Compile time: {:.2}s total, {:.3}s avg",
        stats.total_compile_time.as_secs_f64(),
        avg_compile.as_secs_f64(),
    );
    eprintln!(
        "  Slowest:      {:.2}s ({})",
        stats.max_compile_time.as_secs_f64(),
        stats.max_compile_file,
    );
}

/// The three-line banner prepended to dumped WIR/NIR golden files, identifying
/// the phase, the source fixture, and how to regenerate it.
fn golden_header(title: &str, input_path: &str) -> String {
    format!(
        "// Golden file: {title}\n\
         // Source: {input_path}\n\
         // Generated by: mise run update-golden-fixtures\n\n"
    )
}

/// Compile a single fixture and render every requested phase. The compiler is
/// invoked at most twice — once for the dump phases (WIR/NIR/lowered NIR) and
/// once for WAT — regardless of how many emits share those phases.
async fn render_phases(
    source: &str,
    input_path: &str,
    phases: &[Phase],
    opt_level: OptLevel,
) -> HashMap<Phase, Result<String, String>> {
    let mut out: HashMap<Phase, Result<String, String>> = HashMap::new();

    let needs_dump = phases
        .iter()
        .any(|p| matches!(p, Phase::Wir | Phase::Nir | Phase::NirLowered));
    let needs_wat = phases.contains(&Phase::Wat);

    let path = Path::new(input_path);
    let base_path = path
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_default();
    let target_world = extract_world_from_data_section(source);

    if needs_dump {
        let host = FilesystemCompilerHost::silent(base_path.clone());
        let dumped = wado_compiler::dump_with_host_and_world(
            source,
            &host,
            Some(input_path),
            opt_level,
            target_world.as_deref(),
            None,
            None,
        )
        .await;

        match dumped {
            Err(_) => {
                let e = diagnostics_summary(&host, "compilation failed");
                for p in phases {
                    if matches!(p, Phase::Wir | Phase::Nir | Phase::NirLowered) {
                        out.entry(*p).or_insert_with(|| Err(e.clone()));
                    }
                }
            }
            Ok(result) => {
                for p in phases {
                    match p {
                        Phase::Wir => {
                            out.entry(Phase::Wir).or_insert_with(|| {
                                let Some(ref wir_package) = result.wir_package else {
                                    return Err(diagnostics_summary(
                                        &host,
                                        "dump produced no WIR (resolve or a later phase returned Bail)",
                                    ));
                                };
                                let mut s = golden_header("WIR with -O2 optimization", input_path);
                                s.push_str(&wado_compiler::wir_unparse::unparse_wir(
                                    wir_package,
                                    Some(input_path),
                                ));
                                Ok(s)
                            });
                        }
                        Phase::Nir => {
                            out.entry(Phase::Nir).or_insert_with(|| {
                                let Some(ref project) = result.optimized_package else {
                                    return Err(diagnostics_summary(
                                        &host,
                                        "dump produced no NIR (resolve or a later phase returned Bail)",
                                    ));
                                };
                                let unparsed =
                                    wado_compiler::nir_unparse::unparse_nir_package(project);
                                let content = if let Some(idx) = unparsed.find("\n__DATA__\n") {
                                    &unparsed[..=idx]
                                } else if let Some(idx) = unparsed.find("__DATA__\n") {
                                    &unparsed[..idx]
                                } else {
                                    &unparsed
                                };
                                let mut s =
                                    golden_header("Optimized NIR with -O2 optimization", input_path);
                                s.push_str(content);
                                Ok(s)
                            });
                        }
                        Phase::NirLowered => {
                            out.entry(Phase::NirLowered).or_insert_with(|| {
                                let Some(ref text) = result.lowered_nir_text else {
                                    return Err(diagnostics_summary(
                                        &host,
                                        "dump produced no lowered NIR (resolve or a later phase returned Bail)",
                                    ));
                                };
                                Ok(text.clone())
                            });
                        }
                        Phase::Wat => {}
                    }
                }
            }
        }
    }

    if needs_wat {
        let host = FilesystemCompilerHost::silent(base_path);
        let options = wado_compiler::CompilerOptions {
            opt_level,
            target_world: target_world.clone(),
            log_level: Some(wado_compiler::LogLevel::Off),
            ..wado_compiler::CompilerOptions::default()
        };
        let res =
            match wado_compiler::compile_with_options(source, &host, Some(input_path), options)
                .await
            {
                Err(_) => Err(diagnostics_summary(&host, "compilation failed")),
                Ok(result) => {
                    let mut config = wasmprinter::Config::new();
                    config.fold_instructions(true);
                    let mut wat = String::new();
                    config
                        .print(&result.wasm, &mut wasmprinter::PrintFmtWrite(&mut wat))
                        .map(|()| wat)
                        .map_err(|e| format!("WAT generation failed: {e}"))
                }
            };
        out.insert(Phase::Wat, res);
    }

    out
}

/// Build an error string that attaches buffered diagnostics from a
/// `silent()` host. `dump_with_host_and_world` converts a resolve `Bail`
/// into a `DumpResult` with all post-resolve fields `None`, so the
/// pipeline otherwise sees only an empty output with no context.
fn diagnostics_summary(host: &FilesystemCompilerHost, prefix: &str) -> String {
    let diags = host.diagnostics();
    let errors: Vec<String> = diags
        .iter()
        .filter(|d| {
            matches!(
                d.severity,
                wado_compiler::Severity::Error | wado_compiler::Severity::Fatal
            )
        })
        .map(|d| match &d.span {
            Some(span) => format!("{}:{}:{}: {}", span.file, span.line, span.column, d.message),
            None => d.message.clone(),
        })
        .collect();
    if errors.is_empty() {
        format!("{prefix} (no error diagnostics were emitted; likely a compiler bug)")
    } else {
        format!("{prefix}: {}", errors.join("; "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collect_stale_returns_unwritten_files() {
        let existing = vec![
            ("a".to_string(), PathBuf::from("a.wir.wado")),
            ("b".to_string(), PathBuf::from("b.wir.wado")),
            ("c".to_string(), PathBuf::from("c.wir.wado")),
        ];
        let mut written = HashSet::new();
        written.insert("a".to_string());
        written.insert("c".to_string());

        let stale = collect_stale(existing, &written);
        assert_eq!(stale, vec![PathBuf::from("b.wir.wado")]);
    }

    #[test]
    fn collect_stale_keeps_everything_when_all_written() {
        let existing = vec![
            ("a".to_string(), PathBuf::from("a")),
            ("b".to_string(), PathBuf::from("b")),
        ];
        let mut written = HashSet::new();
        written.insert("a".to_string());
        written.insert("b".to_string());

        assert!(collect_stale(existing, &written).is_empty());
    }

    #[test]
    fn collect_stale_removes_all_when_nothing_written() {
        let existing = vec![
            ("a".to_string(), PathBuf::from("a")),
            ("b".to_string(), PathBuf::from("b")),
        ];
        let written = HashSet::new();

        let stale = collect_stale(existing, &written);
        assert_eq!(stale, vec![PathBuf::from("a"), PathBuf::from("b")]);
    }
}
