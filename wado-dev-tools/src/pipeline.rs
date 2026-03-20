use std::fs;
use std::io::Write;
use std::path::Path;
use std::sync::mpsc;
use std::time::Instant;

use wado_compiler::OptLevel;

use crate::compiler_host::FilesystemCompilerHost;
use crate::data_section::{extract_world_from_data_section, should_skip_file};
use crate::template::Template;

const COMPILER_STACK_SIZE: usize = 16 * 1024 * 1024;

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
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Wir,
    Tir,
}

pub fn run_pipeline(
    in_template: &str,
    out_template: &str,
    phase: Phase,
    opt_level: OptLevel,
) {
    let start = Instant::now();

    let num_workers = std::thread::available_parallelism()
        .map(std::num::NonZero::get)
        .unwrap_or(4)
        .saturating_sub(2)
        .max(2);

    let in_tmpl = Template::parse(in_template);
    let out_tmpl = Template::parse(out_template);

    let files = in_tmpl.discover();
    let total = files.len();

    // Pre-filter: identify skip/compile candidates
    let mut read_items: Vec<ReadItem> = Vec::new();
    let mut skip_count = 0u32;

    for (name, path) in &files {
        let source = match fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) => {
                panic!("Failed to read {}: {e}", path.display());
            }
        };
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
    for _ in 0..num_workers {
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
                            let guard = rx.lock().unwrap();
                            match guard.recv() {
                                Ok(item) => item,
                                Err(_) => break, // channel closed
                            }
                        };

                        let output_path =
                            format!("{}{}{}", out_tmpl_prefix, item.name, out_tmpl_suffix);
                        let content = rt.block_on(compile_one(
                            &item.source,
                            &item.input_path,
                            phase,
                            opt_level,
                        ));

                        if tx
                            .send(WriteItem {
                                output_path,
                                content,
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

    // Writer: runs on main thread
    let mut success_count = 0u32;

    for item in write_rx {
        match item.content {
            Ok(content) => {
                if content.is_empty() {
                    let _ = fs::remove_file(&item.output_path);
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
                success_count += 1;
            }
            Err(e) => {
                let _ = fs::remove_file(&item.output_path);
                panic!(
                    "Golden fixture generation failed: {e}\n\
                     If this test is expected to fail at compile time, add \
                     \"compile_error\" or \"TODO\" to its __DATA__ section."
                );
            }
        }
    }

    // Join threads
    reader.join().expect("reader thread panicked");
    for w in workers {
        w.join().expect("dump worker panicked");
    }

    let elapsed = start.elapsed().as_secs_f64();
    eprintln!(
        "Generated {success_count} files ({skip_count} skipped, {total} total) in {elapsed:.2}s"
    );
    assert_eq!(
        success_count as usize + skip_count as usize,
        total,
        "Mismatch: {success_count} + {skip_count} != {compile_count}"
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
            if let Some(ref wir_module) = result.wir_module {
                writeln!(output, "// Golden file: WIR with -O2 optimization").unwrap();
                writeln!(output, "// Source: {input_path}").unwrap();
                writeln!(output, "// Generated by: mise run update-golden-fixtures").unwrap();
                writeln!(output).unwrap();

                let unparsed =
                    wado_compiler::wir_unparse::unparse_wir(wir_module, Some(input_path));
                write!(output, "{unparsed}").unwrap();
            }
        }
        Phase::Tir => {
            if let Some(ref project) = result.optimized_project {
                for (module_source, module) in &project.tir_modules {
                    if module_source.is_entry_point() {
                        writeln!(
                            output,
                            "// Golden file: Optimized TIR with -O2 optimization"
                        )
                        .unwrap();
                        writeln!(output, "// Source: {input_path}").unwrap();
                        writeln!(output, "// Generated by: mise run update-golden-fixtures")
                            .unwrap();
                        writeln!(output).unwrap();

                        let unparsed = wado_compiler::unparse::unparse_tir(module);
                        let content = if let Some(idx) = unparsed.find("\n__DATA__\n") {
                            &unparsed[..=idx]
                        } else if let Some(idx) = unparsed.find("__DATA__\n") {
                            &unparsed[..idx]
                        } else {
                            &unparsed
                        };
                        write!(output, "{content}").unwrap();
                        break;
                    }
                }
            }
        }
    }

    Ok(String::from_utf8(output).unwrap())
}
