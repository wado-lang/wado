use std::fs;
use std::io::Write;
use std::path::Path;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use wado_compiler::OptLevel;

use crate::compiler_host::FilesystemCompilerHost;
use crate::data_section::{extract_world_from_data_section, should_skip_file};
use crate::template::Template;

const COMPILER_STACK_SIZE: usize = 16 * 1024 * 1024;

/// Default worker count: one third of the logical CPUs (min 1). Each worker
/// owns a 16 MB stack and runs the full compiler plus wasmtime, so saturating
/// every core is wasteful and, on constrained sandboxes, has empirically led
/// to SIGSEGV from resource exhaustion during golden-dump. Halving the CPU
/// count still segfaulted in practice, so we go to one third.
fn default_num_workers() -> usize {
    let logical = std::thread::available_parallelism()
        .map(std::num::NonZero::get)
        .unwrap_or(4);
    (logical / 3).max(1)
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

struct WriteItem {
    output_path: String,
    content: Result<String, String>,
    compile_time: Duration,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Wir,
    Tir,
    TirLowered,
    Wat,
}

pub fn run_pipeline(
    in_template: &str,
    out_template: &str,
    phase: Phase,
    opt_level: OptLevel,
    skip_empty: bool,
) {
    let start = Instant::now();

    #[cfg(unix)]
    install_signal_handlers();

    let num_workers = default_num_workers();

    // Force ACTIVE_FILES initialization with correct worker count
    let _ = &*ACTIVE_FILES;

    let in_tmpl = Template::parse(in_template);
    let out_tmpl = Template::parse(out_template);

    let files = in_tmpl.discover();
    let total = files.len();

    // Pre-filter: identify skip/compile candidates
    let mut read_items: Vec<ReadItem> = Vec::new();
    let mut skip_count = 0u32;

    for (name, path) in &files {
        let source = fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("Failed to read {}: {e}", path.display()));
        if should_skip_file(&source) {
            let output_path = out_tmpl.output_path(name);
            let _ = fs::remove_file(&output_path);
            skip_count += 1;
            continue;
        }
        read_items.push(ReadItem {
            name: name.clone(),
            source,
            input_path: path.to_string_lossy().into_owned(),
        });
    }

    let compile_count = read_items.len();

    // Channels
    let (read_tx, read_rx) = mpsc::sync_channel::<ReadItem>(2 * num_workers);
    let (write_tx, write_rx) = mpsc::sync_channel::<WriteItem>(num_workers);

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
        let out_tmpl_prefix = out_template.split("{name}").next().unwrap().to_string();
        let out_tmpl_suffix = out_template.split("{name}").nth(1).unwrap().to_string();

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

                        let output_path =
                            format!("{}{}{}", out_tmpl_prefix, item.name, out_tmpl_suffix);
                        let input_path = item.input_path.clone();

                        // Track active compilation for signal handler
                        let t0 = Instant::now();
                        if let Some(slot) = ACTIVE_FILES.get(worker_id) {
                            *slot.lock().unwrap() = Some((input_path.clone(), t0));
                        }

                        let content =
                            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                rt.block_on(compile_one(
                                    &item.source,
                                    &item.input_path,
                                    phase,
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
                                Err(format!("panic in {input_path}: {msg}"))
                            });
                        let compile_time = t0.elapsed();

                        // Clear active file
                        if let Some(slot) = ACTIVE_FILES.get(worker_id) {
                            *slot.lock().unwrap() = None;
                        }

                        if tx
                            .send(WriteItem {
                                output_path,
                                content,
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

    let stats = collect_results(write_rx, skip_empty, skip_count);

    reader.join().expect("reader thread panicked");
    for w in workers {
        w.join().expect("dump worker panicked");
    }

    assert_eq!(
        stats.success_count as usize + stats.skip_count as usize,
        total,
        "Mismatch: {} + {} != {compile_count}",
        stats.success_count,
        stats.skip_count
    );

    print_stats(&stats, total, num_workers, start.elapsed());
}

struct PipelineStats {
    success_count: u32,
    skip_count: u32,
    total_compile_time: Duration,
    max_compile_time: Duration,
    max_compile_file: String,
}

fn collect_results(
    write_rx: mpsc::Receiver<WriteItem>,
    skip_empty: bool,
    mut skip_count: u32,
) -> PipelineStats {
    let mut success_count = 0u32;
    let mut total_compile_time = Duration::ZERO;
    let mut max_compile_time = Duration::ZERO;
    let mut max_compile_file = String::new();

    for item in write_rx {
        match item.content {
            Ok(content) => {
                if content.is_empty() {
                    let _ = fs::remove_file(&item.output_path);
                    if skip_empty {
                        eprintln!("  Skipped {} (empty output)", item.output_path);
                        skip_count += 1;
                        continue;
                    }
                    panic!(
                        "Golden fixture generation failed: Empty output for {} (skipping)\n\
                         If this test is expected to fail at compile time, add \
                         \"compile_error\" or \"TODO\" to its __DATA__ section.",
                        item.output_path
                    );
                }
                if let Some(parent) = Path::new(&item.output_path).parent()
                    && !parent.as_os_str().is_empty()
                {
                    let _ = fs::create_dir_all(parent);
                }
                fs::write(&item.output_path, &content)
                    .unwrap_or_else(|e| panic!("Failed to write {}: {e}", item.output_path));
                eprintln!("  Generated {}", item.output_path);
                total_compile_time += item.compile_time;
                if item.compile_time > max_compile_time {
                    max_compile_time = item.compile_time;
                    max_compile_file.clone_from(&item.output_path);
                }
                success_count += 1;
            }
            Err(e) => {
                let _ = fs::remove_file(&item.output_path);
                if skip_empty {
                    eprintln!("  Skipped {} ({e})", item.output_path);
                    skip_count += 1;
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

    PipelineStats {
        success_count,
        skip_count,
        total_compile_time,
        max_compile_time,
        max_compile_file,
    }
}

fn print_stats(stats: &PipelineStats, total: usize, num_workers: usize, elapsed: Duration) {
    let avg_compile = if stats.success_count > 0 {
        stats.total_compile_time / stats.success_count
    } else {
        Duration::ZERO
    };
    eprintln!();
    eprintln!(
        "  Files:        {} generated, {} skipped, {total} total",
        stats.success_count, stats.skip_count
    );
    eprintln!("  Workers:      {num_workers}");
    eprintln!("  Wall time:    {:.2}s", elapsed.as_secs_f64());
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

async fn compile_one(
    source: &str,
    input_path: &str,
    phase: Phase,
    opt_level: OptLevel,
) -> Result<String, String> {
    let path = Path::new(input_path);
    let base_path = path
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_default();
    let host = FilesystemCompilerHost::silent(base_path);

    let target_world = extract_world_from_data_section(source);

    if phase == Phase::Wat {
        let options = wado_compiler::CompilerOptions {
            opt_level,
            target_world: target_world.clone(),
            log_level: Some(wado_compiler::LogLevel::Off),
            ..wado_compiler::CompilerOptions::default()
        };
        let result = wado_compiler::compile_with_options(source, &host, Some(input_path), options)
            .await
            .map_err(|_| "compilation failed".to_string())?;
        let mut config = wasmprinter::Config::new();
        config.fold_instructions(true);
        let mut wat = String::new();
        config
            .print(&result.wasm, &mut wasmprinter::PrintFmtWrite(&mut wat))
            .map_err(|e| format!("WAT generation failed: {e}"))?;
        return Ok(wat);
    }

    let result = wado_compiler::dump_with_host_and_world(
        source,
        &host,
        Some(input_path),
        opt_level,
        target_world.as_deref(),
        None,
        None,
    )
    .await
    .map_err(|_| "compilation failed".to_string())?;

    let mut output = Vec::new();

    match phase {
        Phase::Wir => {
            if let Some(ref wir_package) = result.wir_package {
                writeln!(output, "// Golden file: WIR with -O2 optimization").unwrap();
                writeln!(output, "// Source: {input_path}").unwrap();
                writeln!(output, "// Generated by: mise run update-golden-fixtures").unwrap();
                writeln!(output).unwrap();

                let unparsed =
                    wado_compiler::wir_unparse::unparse_wir(wir_package, Some(input_path));
                write!(output, "{unparsed}").unwrap();
            }
        }
        Phase::Tir => {
            if let Some(ref project) = result.optimized_package {
                writeln!(
                    output,
                    "// Golden file: Optimized TIR with -O2 optimization"
                )
                .unwrap();
                writeln!(output, "// Source: {input_path}").unwrap();
                writeln!(output, "// Generated by: mise run update-golden-fixtures").unwrap();
                writeln!(output).unwrap();

                let unparsed = wado_compiler::unparse::unparse_flat_package(project);
                let content = if let Some(idx) = unparsed.find("\n__DATA__\n") {
                    &unparsed[..=idx]
                } else if let Some(idx) = unparsed.find("__DATA__\n") {
                    &unparsed[..idx]
                } else {
                    &unparsed
                };
                write!(output, "{content}").unwrap();
            }
        }
        Phase::TirLowered => {
            if let Some(ref text) = result.lowered_tir_text {
                write!(output, "{text}").unwrap();
            }
        }
        Phase::Wat => unreachable!(),
    }

    Ok(String::from_utf8(output).unwrap())
}
