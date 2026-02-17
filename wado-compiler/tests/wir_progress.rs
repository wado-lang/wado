//! WIR pipeline progress tracker.
//!
//! Runs all E2E fixtures through the WIR pipeline and reports pass/fail counts.
//! This test always succeeds — it's informational only.
//!
//! Usage:
//!   WADO_WIR_TEST=1 cargo test -p wado-compiler --test wir_progress -- --nocapture

mod common;

use serde::Deserialize;
use std::path::Path;
use wado_compiler::{CompilerOptions, OptLevel};

#[derive(Debug, Deserialize, Default)]
struct TestSpec {
    #[serde(default)]
    stdout: Option<String>,
    #[serde(default)]
    stderr: Option<String>,
    #[serde(default)]
    stdout_contains: Vec<String>,
    #[serde(default)]
    stderr_contains: Vec<String>,
    #[serde(default)]
    trapped: bool,
    #[serde(default)]
    compile_error: Option<String>,
    #[serde(default)]
    #[serde(rename = "TODO")]
    todo: bool,
}

#[derive(Debug, Default)]
struct ProgressStats {
    total: usize,
    passed: usize,
    failed_compile: usize,
    failed_runtime: usize,
    failed_output: usize,
    failed_todo: usize,
    skipped_todo: usize,
}

fn try_run_fixture(path: &Path, source: &str, opt_level: OptLevel) -> Result<(), String> {
    let data_section =
        common::extract_data_section(source).ok_or_else(|| "missing __DATA__".to_string())?;
    let spec: TestSpec =
        serde_json::from_str(data_section).map_err(|e| format!("invalid __DATA__ JSON: {e}"))?;

    let options = CompilerOptions {
        opt_level,
        use_wir_backend: true,
        ..CompilerOptions::default()
    };

    let compile_result = common::compile_source_with_compiler_options(path, source, options);

    // Handle expected compile errors
    if let Some(expected_error) = &spec.compile_error {
        return match compile_result {
            Err(e) => {
                let error_msg = e.to_string();
                if error_msg.contains(expected_error) {
                    Ok(())
                } else {
                    Err(format!(
                        "compile error mismatch: expected '{expected_error}', got '{error_msg}'"
                    ))
                }
            }
            Ok(_) => Err(format!(
                "expected compile error '{expected_error}', but succeeded"
            )),
        };
    }

    let compile_result = compile_result.map_err(|e| format!("compile error: {e}"))?;

    let result =
        common::run_wasm(compile_result.wasm).map_err(|e| format!("runtime error: {e}"))?;

    // Check trapped
    if result.trapped != spec.trapped {
        let stderr_info = if !result.stderr.is_empty() {
            format!(" stderr={:?}", result.stderr)
        } else {
            String::new()
        };
        return Err(format!(
            "trapped mismatch: expected {}, got {}{}",
            spec.trapped, result.trapped, stderr_info
        ));
    }

    // Check stdout
    if let Some(expected_stdout) = &spec.stdout {
        if &result.stdout != expected_stdout {
            return Err(format!(
                "stdout mismatch: expected {:?}, got {:?}",
                expected_stdout, result.stdout
            ));
        }
    }

    // Check stderr
    if let Some(expected_stderr) = &spec.stderr {
        if &result.stderr != expected_stderr {
            return Err(format!(
                "stderr mismatch: expected {:?}, got {:?}",
                expected_stderr, result.stderr
            ));
        }
    }

    // Check stdout_contains
    for expected in &spec.stdout_contains {
        if !result.stdout.contains(expected) {
            return Err(format!("stdout missing '{expected}'"));
        }
    }

    // Check stderr_contains
    for expected in &spec.stderr_contains {
        if !result.stderr.contains(expected) {
            return Err(format!("stderr missing '{expected}'"));
        }
    }

    Ok(())
}

#[test]
fn wir_pipeline_progress() {
    if std::env::var("WADO_WIR_TEST").is_err() {
        eprintln!("WIR progress: skipped (set WADO_WIR_TEST=1 to run)");
        return;
    }

    let fixtures_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let mut entries: Vec<_> = std::fs::read_dir(&fixtures_dir)
        .expect("fixtures directory should exist")
        .filter_map(|e| e.ok())
        .filter(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            name.ends_with(".wado") && !name.contains('/')
        })
        .collect();
    entries.sort_by_key(std::fs::DirEntry::file_name);

    let opt_level = OptLevel::O2;
    let mut stats = ProgressStats::default();
    let mut failures: Vec<(String, String)> = Vec::new();

    for entry in &entries {
        let path = entry.path();
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        let source = std::fs::read_to_string(&path).unwrap();

        // Check if TODO test
        let data_section = common::extract_data_section(&source);
        let is_todo = data_section
            .and_then(|d| serde_json::from_str::<TestSpec>(d).ok())
            .is_some_and(|s| s.todo);

        stats.total += 1;

        if is_todo {
            // For TODO tests, we expect failure — run it and check
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                try_run_fixture(&path, &source, opt_level)
            }));

            match result {
                Ok(Ok(())) => {
                    // TODO test passed — this means the feature works now
                    stats.failed_todo += 1;
                    failures.push((name, "TODO test unexpectedly passed".to_string()));
                }
                Ok(Err(_)) | Err(_) => {
                    // TODO test failed as expected
                    stats.skipped_todo += 1;
                }
            }
            continue;
        }

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            try_run_fixture(&path, &source, opt_level)
        }));

        match result {
            Ok(Ok(())) => {
                stats.passed += 1;
            }
            Ok(Err(msg)) => {
                if msg.starts_with("compile error:") {
                    stats.failed_compile += 1;
                } else if msg.starts_with("runtime error:") {
                    stats.failed_runtime += 1;
                } else {
                    stats.failed_output += 1;
                }
                failures.push((name, msg));
            }
            Err(panic_info) => {
                let msg = panic_info
                    .downcast_ref::<String>()
                    .map(|s| s.as_str())
                    .or_else(|| panic_info.downcast_ref::<&str>().copied())
                    .unwrap_or("(panic)")
                    .to_string();
                stats.failed_runtime += 1;
                failures.push((name, msg));
            }
        }
    }

    // Print summary
    let non_todo_total = stats.total - stats.skipped_todo - stats.failed_todo;
    let failed = non_todo_total - stats.passed;
    let pct = if non_todo_total == 0 {
        0.0
    } else {
        (stats.passed as f64 / non_todo_total as f64) * 100.0
    };

    eprintln!();
    eprintln!("═══════════════════════════════════════════════════════");
    eprintln!("  WIR Pipeline Progress (O2)");
    eprintln!("═══════════════════════════════════════════════════════");
    eprintln!(
        "  Passed:  {:>4} / {:<4} ({pct:.1}%)",
        stats.passed, non_todo_total
    );
    eprintln!("  Failed:  {:>4}", failed);
    if stats.failed_compile > 0 {
        eprintln!("    compile: {:>4}", stats.failed_compile);
    }
    if stats.failed_runtime > 0 {
        eprintln!("    runtime: {:>4}", stats.failed_runtime);
    }
    if stats.failed_output > 0 {
        eprintln!("    output:  {:>4}", stats.failed_output);
    }
    if stats.skipped_todo > 0 {
        eprintln!("  TODO (skipped): {:>4}", stats.skipped_todo);
    }
    if stats.failed_todo > 0 {
        eprintln!("  TODO (unexpectedly passed): {:>4}", stats.failed_todo);
    }
    eprintln!("═══════════════════════════════════════════════════════");

    // Print first few failures for quick diagnosis
    if !failures.is_empty() {
        let show = failures.len().min(500);
        eprintln!();
        eprintln!("  First {show} failures:");
        for (name, msg) in failures.iter().take(show) {
            let short_msg = if msg.len() > 400 { &msg[..400] } else { msg };
            eprintln!("    {name}: {short_msg}");
        }
        if failures.len() > show {
            eprintln!("    ... and {} more", failures.len() - show);
        }
    }
    eprintln!();

    // This test always passes — it's informational only
}
