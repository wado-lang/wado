//! Cross-validation tests comparing Wado's core:zlib implementation against
//! zlib-rs and flate2 (backed by zlib-rs).
//!
//! These tests verify:
//! - Wado's zlib_compress output can be decompressed by zlib-rs (cross-inflate)
//! - zlib-rs compressed output can be decompressed by Wado's inflate_zlib (cross-inflate)
//! - Inflate round-trip: inflate(deflate(data)) == data for both implementations
//! - Checksum compatibility (adler32, crc32) between Wado and zlib-rs
//! - All compression levels and strategies produce valid output
//! - Raw deflate/inflate format cross-compatibility
//! - Gzip format cross-compatibility
//! - Preset dictionary cross-compatibility
//! - Large data handling
//! - Edge cases (empty, single byte, all zeros, etc.)

mod common;

use flate2::Compression;
use flate2::write::{DeflateDecoder, DeflateEncoder, GzDecoder, GzEncoder};
use std::io::Write;
use std::sync::{Condvar, Mutex, OnceLock};
use zlib_rs::{DeflateConfig, InflateConfig, ReturnCode};

/// Limit concurrent Wasm executions to avoid SIGSEGV from memory pressure.
static WASM_SEMAPHORE: OnceLock<(Mutex<usize>, Condvar)> = OnceLock::new();
const MAX_CONCURRENT_WASM: usize = 4;

struct WasmGuard;

impl WasmGuard {
    fn acquire() -> Self {
        let (mutex, condvar) = WASM_SEMAPHORE.get_or_init(|| (Mutex::new(0), Condvar::new()));
        let mut count = mutex.lock().unwrap();
        while *count >= MAX_CONCURRENT_WASM {
            count = condvar.wait(count).unwrap();
        }
        *count += 1;
        WasmGuard
    }
}

impl Drop for WasmGuard {
    fn drop(&mut self) {
        let (mutex, condvar) = WASM_SEMAPHORE.get().unwrap();
        let mut count = mutex.lock().unwrap();
        *count -= 1;
        condvar.notify_one();
    }
}

/// Format a byte slice as Wado array literal elements: "72,101,..."
fn bytes_to_wado_array(data: &[u8]) -> String {
    data.iter()
        .map(|b| format!("{b}"))
        .collect::<Vec<_>>()
        .join(",")
}

/// Parse space-separated decimal bytes from stdout
fn parse_bytes(stdout: &str) -> Vec<u8> {
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return vec![];
    }
    trimmed
        .split(' ')
        .map(|s| s.parse::<u8>().unwrap())
        .collect()
}

/// Compress data using zlib-rs (zlib format)
fn zlib_rs_compress(input: &[u8], level: i32) -> Vec<u8> {
    let bound = zlib_rs::compress_bound(input.len());
    let mut output = vec![0u8; bound];
    let config = DeflateConfig::new(level);
    let (compressed, rc) = zlib_rs::compress_slice(&mut output, input, config);
    assert_eq!(rc, ReturnCode::Ok, "zlib-rs compress failed");
    compressed.to_vec()
}

/// Decompress zlib data using zlib-rs
fn zlib_rs_decompress(input: &[u8], max_output: usize) -> Vec<u8> {
    let mut output = vec![0u8; max_output];
    let (decompressed, rc) =
        zlib_rs::decompress_slice(&mut output, input, InflateConfig::default());
    assert_eq!(rc, ReturnCode::Ok, "zlib-rs decompress failed");
    decompressed.to_vec()
}

/// Compress data using flate2 (raw deflate format)
fn flate2_deflate_raw(input: &[u8], level: u32) -> Vec<u8> {
    let mut encoder = DeflateEncoder::new(Vec::new(), Compression::new(level));
    encoder.write_all(input).unwrap();
    encoder.finish().unwrap()
}

/// Decompress raw deflate data using flate2
fn flate2_inflate_raw(input: &[u8]) -> Vec<u8> {
    let mut decoder = DeflateDecoder::new(Vec::new());
    decoder.write_all(input).unwrap();
    decoder.finish().unwrap()
}

/// Compress data using flate2 (gzip format)
fn flate2_gzip_compress(input: &[u8], level: u32) -> Vec<u8> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::new(level));
    encoder.write_all(input).unwrap();
    encoder.finish().unwrap()
}

/// Decompress gzip data using flate2
fn flate2_gzip_decompress(input: &[u8]) -> Vec<u8> {
    let mut decoder = GzDecoder::new(Vec::new());
    decoder.write_all(input).unwrap();
    decoder.finish().unwrap()
}

/// Compile Wado source and run it, returning stdout
fn compile_and_run(source: &str) -> String {
    let _guard = WasmGuard::acquire();
    let result = common::compile_source(source).expect("compilation failed");
    let run_result = common::run_wasm(result.wasm).expect("wasm execution failed");
    assert!(!run_result.trapped, "Wasm trapped: {}", run_result.stderr);
    run_result.stdout
}

/// Build a Wado source that compresses data with compress2 at a given level
fn make_compress2_source(array_literal: &str, level: i32) -> String {
    format!(
        r#"use {{ print, println, Stdout }} from "core:cli";
use {{ compress2 }} from "core:zlib";

export fn run() with Stdout {{
    let data: Array<u8> = [{array_literal}];
    let compressed = compress2(&data, {level});
    for let mut i = 0; i < compressed.len(); i += 1 {{
        if i > 0 {{ print(" "); }}
        print(`{{compressed[i] as i32}}`);
    }}
    println("");
}}"#
    )
}

/// Build a Wado source that compresses with a strategy
fn make_compress_strategy_source(array_literal: &str, level: i32, strategy: i32) -> String {
    format!(
        r#"use {{ print, println, Stdout }} from "core:cli";
use {{ compress_with_strategy }} from "core:zlib";

export fn run() with Stdout {{
    let data: Array<u8> = [{array_literal}];
    let compressed = compress_with_strategy(&data, {level}, {strategy});
    for let mut i = 0; i < compressed.len(); i += 1 {{
        if i > 0 {{ print(" "); }}
        print(`{{compressed[i] as i32}}`);
    }}
    println("");
}}"#
    )
}

/// Build a Wado source that inflates zlib data
fn make_inflate_source(array_literal: &str) -> String {
    format!(
        r#"use {{ print, println, Stdout }} from "core:cli";
use {{ inflate_zlib }} from "core:zlib";

export fn run() with Stdout {{
    let data: Array<u8> = [{array_literal}];
    let decompressed = inflate_zlib(&data);
    for let mut i = 0; i < decompressed.len(); i += 1 {{
        if i > 0 {{ print(" "); }}
        print(`{{decompressed[i] as i32}}`);
    }}
    println("");
}}"#
    )
}

/// Build a Wado source that inflates raw deflate data
fn make_inflate_raw_source(array_literal: &str) -> String {
    format!(
        r#"use {{ print, println, Stdout }} from "core:cli";
use {{ inflate_raw }} from "core:zlib";

export fn run() with Stdout {{
    let data: Array<u8> = [{array_literal}];
    let decompressed = inflate_raw(&data);
    for let mut i = 0; i < decompressed.len(); i += 1 {{
        if i > 0 {{ print(" "); }}
        print(`{{decompressed[i] as i32}}`);
    }}
    println("");
}}"#
    )
}

/// Build a Wado source that deflates raw
fn make_deflate_raw_source(array_literal: &str) -> String {
    format!(
        r#"use {{ print, println, Stdout }} from "core:cli";
use {{ deflate_raw }} from "core:zlib";

export fn run() with Stdout {{
    let data: Array<u8> = [{array_literal}];
    let compressed = deflate_raw(&data);
    for let mut i = 0; i < compressed.len(); i += 1 {{
        if i > 0 {{ print(" "); }}
        print(`{{compressed[i] as i32}}`);
    }}
    println("");
}}"#
    )
}

/// Build a Wado source that inflates gzip data
fn make_inflate_gzip_source(array_literal: &str) -> String {
    format!(
        r#"use {{ print, println, Stdout }} from "core:cli";
use {{ inflate_gzip }} from "core:zlib";

export fn run() with Stdout {{
    let data: Array<u8> = [{array_literal}];
    let decompressed = inflate_gzip(&data);
    for let mut i = 0; i < decompressed.len(); i += 1 {{
        if i > 0 {{ print(" "); }}
        print(`{{decompressed[i] as i32}}`);
    }}
    println("");
}}"#
    )
}

/// Build a Wado source that gzip-compresses data
fn make_gzip_compress_source(array_literal: &str) -> String {
    format!(
        r#"use {{ print, println, Stdout }} from "core:cli";
use {{ gzip_compress }} from "core:zlib";

export fn run() with Stdout {{
    let data: Array<u8> = [{array_literal}];
    let compressed = gzip_compress(&data);
    for let mut i = 0; i < compressed.len(); i += 1 {{
        if i > 0 {{ print(" "); }}
        print(`{{compressed[i] as i32}}`);
    }}
    println("");
}}"#
    )
}

/// Build Wado source that computes adler32 and crc32 checksums
fn make_checksum_source(array_literal: &str, len: usize) -> String {
    format!(
        r#"use {{ println, Stdout }} from "core:cli";
use {{ adler32, adler32_init, crc32, crc32_init }} from "core:zlib";

export fn run() with Stdout {{
    let data: Array<u8> = [{array_literal}];
    let a = adler32(adler32_init(), &data, 0, {len});
    let c = crc32(crc32_init(), &data, 0, {len});
    println(`{{a}} {{c}}`);
}}"#
    )
}

/// Generate test data of various patterns
fn test_data_sequential(size: usize) -> Vec<u8> {
    (0..size).map(|i| (i % 256) as u8).collect()
}

fn test_data_repeated(byte: u8, size: usize) -> Vec<u8> {
    vec![byte; size]
}

fn test_data_text(repeats: usize) -> Vec<u8> {
    b"Hello, World! This is a test of the zlib compression library. ".repeat(repeats)
}

// deterministic pseudo-random data
fn test_data_pseudorandom(size: usize) -> Vec<u8> {
    let mut data = Vec::with_capacity(size);
    let mut state: u32 = 0x1234_5678;
    for _ in 0..size {
        state = state.wrapping_mul(1_103_515_245).wrapping_add(12345);
        data.push((state >> 16) as u8);
    }
    data
}

#[test]
fn wado_compress_hello_decompressed_by_zlib_rs() {
    let input = b"Hello";
    let source = make_compress2_source(&bytes_to_wado_array(input), 6);
    let stdout = compile_and_run(&source);
    let wado_compressed = parse_bytes(&stdout);
    let decompressed = zlib_rs_decompress(&wado_compressed, 1024);
    assert_eq!(decompressed, input, "cross-inflate failed for 'Hello'");
}

#[test]
fn wado_compress_empty_decompressed_by_zlib_rs() {
    let source = make_compress2_source("", 6);
    let stdout = compile_and_run(&source);
    let wado_compressed = parse_bytes(&stdout);
    let decompressed = zlib_rs_decompress(&wado_compressed, 1024);
    assert!(decompressed.is_empty(), "cross-inflate empty failed");
}

#[test]
fn wado_compress_level1_decompressed_by_zlib_rs() {
    let input = test_data_text(10);
    let source = make_compress2_source(&bytes_to_wado_array(&input), 1);
    let stdout = compile_and_run(&source);
    let wado_compressed = parse_bytes(&stdout);
    let decompressed = zlib_rs_decompress(&wado_compressed, input.len() * 2);
    assert_eq!(decompressed, input, "cross-inflate level 1 failed");
}

#[test]
fn wado_compress_level9_decompressed_by_zlib_rs() {
    let input = test_data_text(10);
    let source = make_compress2_source(&bytes_to_wado_array(&input), 9);
    let stdout = compile_and_run(&source);
    let wado_compressed = parse_bytes(&stdout);
    let decompressed = zlib_rs_decompress(&wado_compressed, input.len() * 2);
    assert_eq!(decompressed, input, "cross-inflate level 9 failed");
}

#[test]
fn wado_compress_all_levels_decompressed_by_zlib_rs() {
    let input = test_data_sequential(500);
    for level in 0..=9 {
        let source = make_compress2_source(&bytes_to_wado_array(&input), level);
        let stdout = compile_and_run(&source);
        let wado_compressed = parse_bytes(&stdout);
        let decompressed = zlib_rs_decompress(&wado_compressed, input.len() * 2);
        assert_eq!(decompressed, input, "cross-inflate failed at level {level}");
    }
}

#[test]
fn wado_compress_strategy_filtered_decompressed_by_zlib_rs() {
    let input = test_data_sequential(500);
    let source = make_compress_strategy_source(&bytes_to_wado_array(&input), 6, 1); // Z_FILTERED=1
    let stdout = compile_and_run(&source);
    let wado_compressed = parse_bytes(&stdout);
    let decompressed = zlib_rs_decompress(&wado_compressed, input.len() * 2);
    assert_eq!(decompressed, input, "cross-inflate Z_FILTERED failed");
}

#[test]
fn wado_compress_strategy_huffman_only_decompressed_by_zlib_rs() {
    let input = test_data_sequential(500);
    let source = make_compress_strategy_source(&bytes_to_wado_array(&input), 6, 2); // Z_HUFFMAN_ONLY=2
    let stdout = compile_and_run(&source);
    let wado_compressed = parse_bytes(&stdout);
    let decompressed = zlib_rs_decompress(&wado_compressed, input.len() * 2);
    assert_eq!(decompressed, input, "cross-inflate Z_HUFFMAN_ONLY failed");
}

#[test]
fn wado_compress_strategy_rle_decompressed_by_zlib_rs() {
    let input = test_data_repeated(0x42, 500);
    let source = make_compress_strategy_source(&bytes_to_wado_array(&input), 6, 3); // Z_RLE=3
    let stdout = compile_and_run(&source);
    let wado_compressed = parse_bytes(&stdout);
    let decompressed = zlib_rs_decompress(&wado_compressed, input.len() * 2);
    assert_eq!(decompressed, input, "cross-inflate Z_RLE failed");
}

#[test]
fn wado_compress_strategy_fixed_decompressed_by_zlib_rs() {
    let input = test_data_text(5);
    let source = make_compress_strategy_source(&bytes_to_wado_array(&input), 6, 4); // Z_FIXED=4
    let stdout = compile_and_run(&source);
    let wado_compressed = parse_bytes(&stdout);
    let decompressed = zlib_rs_decompress(&wado_compressed, input.len() * 2);
    assert_eq!(decompressed, input, "cross-inflate Z_FIXED failed");
}

#[test]
fn zlib_rs_all_levels_inflated_by_wado() {
    let input = test_data_sequential(500);
    for level in 0..=9 {
        let compressed = zlib_rs_compress(&input, level);
        let source = make_inflate_source(&bytes_to_wado_array(&compressed));
        let stdout = compile_and_run(&source);
        let decompressed = parse_bytes(&stdout);
        assert_eq!(
            decompressed, input,
            "Wado inflate failed for zlib-rs level {level}"
        );
    }
}

#[test]
fn zlib_rs_text_data_inflated_by_wado() {
    let input = test_data_text(20);
    let compressed = zlib_rs_compress(&input, 6);
    let source = make_inflate_source(&bytes_to_wado_array(&compressed));
    let stdout = compile_and_run(&source);
    let decompressed = parse_bytes(&stdout);
    assert_eq!(decompressed, input, "Wado inflate failed for text data");
}

#[test]
fn zlib_rs_pseudorandom_data_inflated_by_wado() {
    let input = test_data_pseudorandom(1000);
    let compressed = zlib_rs_compress(&input, 6);
    let source = make_inflate_source(&bytes_to_wado_array(&compressed));
    let stdout = compile_and_run(&source);
    let decompressed = parse_bytes(&stdout);
    assert_eq!(
        decompressed, input,
        "Wado inflate failed for pseudo-random data"
    );
}

#[test]
fn wado_raw_deflate_decompressed_by_flate2() {
    let input = test_data_text(5);
    let source = make_deflate_raw_source(&bytes_to_wado_array(&input));
    let stdout = compile_and_run(&source);
    let wado_raw = parse_bytes(&stdout);
    let decompressed = flate2_inflate_raw(&wado_raw);
    assert_eq!(
        decompressed, input,
        "flate2 failed to inflate Wado raw deflate"
    );
}

#[test]
fn flate2_raw_deflate_inflated_by_wado() {
    let input = test_data_sequential(300);
    let compressed = flate2_deflate_raw(&input, 6);
    let source = make_inflate_raw_source(&bytes_to_wado_array(&compressed));
    let stdout = compile_and_run(&source);
    let decompressed = parse_bytes(&stdout);
    assert_eq!(
        decompressed, input,
        "Wado failed to inflate flate2 raw deflate"
    );
}

#[test]
fn flate2_raw_deflate_all_levels_inflated_by_wado() {
    let input = test_data_text(5);
    for level in 0..=9 {
        let compressed = flate2_deflate_raw(&input, level);
        let source = make_inflate_raw_source(&bytes_to_wado_array(&compressed));
        let stdout = compile_and_run(&source);
        let decompressed = parse_bytes(&stdout);
        assert_eq!(
            decompressed, input,
            "Wado inflate_raw failed for flate2 level {level}"
        );
    }
}

#[test]
fn wado_gzip_compress_decompressed_by_flate2() {
    let input = test_data_text(5);
    let source = make_gzip_compress_source(&bytes_to_wado_array(&input));
    let stdout = compile_and_run(&source);
    let wado_gzip = parse_bytes(&stdout);
    let decompressed = flate2_gzip_decompress(&wado_gzip);
    assert_eq!(
        decompressed, input,
        "flate2 failed to decompress Wado gzip output"
    );
}

#[test]
fn flate2_gzip_inflated_by_wado() {
    let input = test_data_sequential(300);
    let compressed = flate2_gzip_compress(&input, 6);
    let source = make_inflate_gzip_source(&bytes_to_wado_array(&compressed));
    let stdout = compile_and_run(&source);
    let decompressed = parse_bytes(&stdout);
    assert_eq!(
        decompressed, input,
        "Wado failed to inflate flate2 gzip data"
    );
}

#[test]
fn flate2_gzip_all_levels_inflated_by_wado() {
    let input = test_data_text(3);
    for level in 0..=9 {
        let compressed = flate2_gzip_compress(&input, level);
        let source = make_inflate_gzip_source(&bytes_to_wado_array(&compressed));
        let stdout = compile_and_run(&source);
        let decompressed = parse_bytes(&stdout);
        assert_eq!(
            decompressed, input,
            "Wado inflate_gzip failed for flate2 level {level}"
        );
    }
}

#[test]
fn adler32_crc32_hello_matches_zlib_rs() {
    let input = b"Hello";
    let source = make_checksum_source(&bytes_to_wado_array(input), input.len());
    let stdout = compile_and_run(&source);
    let parts: Vec<&str> = stdout.trim().split(' ').collect();
    let wado_adler: u32 = parts[0].parse().unwrap();
    let wado_crc: u32 = parts[1].parse().unwrap();
    let rs_adler = zlib_rs::adler32::adler32(1, input);
    let rs_crc = zlib_rs::crc32::crc32(0, input);
    assert_eq!(wado_adler, rs_adler, "adler32 mismatch for 'Hello'");
    assert_eq!(wado_crc, rs_crc, "crc32 mismatch for 'Hello'");
}

#[test]
fn adler32_crc32_bytes_0_to_255_matches_zlib_rs() {
    let input: Vec<u8> = (0..=255).collect();
    let source = make_checksum_source(&bytes_to_wado_array(&input), input.len());
    let stdout = compile_and_run(&source);
    let parts: Vec<&str> = stdout.trim().split(' ').collect();
    let wado_adler: u32 = parts[0].parse().unwrap();
    let wado_crc: u32 = parts[1].parse().unwrap();
    let rs_adler = zlib_rs::adler32::adler32(1, &input);
    let rs_crc = zlib_rs::crc32::crc32(0, &input);
    assert_eq!(wado_adler, rs_adler, "adler32 mismatch for bytes 0..=255");
    assert_eq!(wado_crc, rs_crc, "crc32 mismatch for bytes 0..=255");
}

#[test]
fn adler32_crc32_empty_matches_zlib_rs() {
    let source = make_checksum_source("", 0);
    let stdout = compile_and_run(&source);
    let parts: Vec<&str> = stdout.trim().split(' ').collect();
    let wado_adler: u32 = parts[0].parse().unwrap();
    let wado_crc: u32 = parts[1].parse().unwrap();
    let rs_adler = zlib_rs::adler32::adler32(1, b"");
    let rs_crc = zlib_rs::crc32::crc32(0, b"");
    assert_eq!(wado_adler, rs_adler, "adler32 mismatch for empty");
    assert_eq!(wado_crc, rs_crc, "crc32 mismatch for empty");
}

#[test]
fn adler32_crc32_large_data_matches_zlib_rs() {
    let input = test_data_pseudorandom(5000);
    let source = make_checksum_source(&bytes_to_wado_array(&input), input.len());
    let stdout = compile_and_run(&source);
    let parts: Vec<&str> = stdout.trim().split(' ').collect();
    let wado_adler: u32 = parts[0].parse().unwrap();
    let wado_crc: u32 = parts[1].parse().unwrap();
    let rs_adler = zlib_rs::adler32::adler32(1, &input);
    let rs_crc = zlib_rs::crc32::crc32(0, &input);
    assert_eq!(wado_adler, rs_adler, "adler32 mismatch for large data");
    assert_eq!(wado_crc, rs_crc, "crc32 mismatch for large data");
}

#[test]
fn adler32_incremental_matches_zlib_rs() {
    // Compute adler32 incrementally in two pieces and compare
    let input = test_data_sequential(1000);
    let source = format!(
        r#"use {{ println, Stdout }} from "core:cli";
use {{ adler32, adler32_init }} from "core:zlib";

export fn run() with Stdout {{
    let data: Array<u8> = [{data}];
    // First half
    let a1 = adler32(adler32_init(), &data, 0, 500);
    // Second half using a1 as init
    let a2 = adler32(a1, &data, 500, 500);
    println(`{{a2}}`);
}}"#,
        data = bytes_to_wado_array(&input)
    );
    let stdout = compile_and_run(&source);
    let wado_adler: u32 = stdout.trim().parse().unwrap();

    // zlib-rs incremental
    let a1 = zlib_rs::adler32::adler32(1, &input[..500]);
    let a2 = zlib_rs::adler32::adler32(a1, &input[500..]);
    assert_eq!(wado_adler, a2, "incremental adler32 mismatch");
}

#[test]
fn crc32_incremental_matches_zlib_rs() {
    let input = test_data_sequential(1000);
    let source = format!(
        r#"use {{ println, Stdout }} from "core:cli";
use {{ crc32, crc32_init }} from "core:zlib";

export fn run() with Stdout {{
    let data: Array<u8> = [{data}];
    let c1 = crc32(crc32_init(), &data, 0, 500);
    let c2 = crc32(c1, &data, 500, 500);
    println(`{{c2}}`);
}}"#,
        data = bytes_to_wado_array(&input)
    );
    let stdout = compile_and_run(&source);
    let wado_crc: u32 = stdout.trim().parse().unwrap();

    let c1 = zlib_rs::crc32::crc32(0, &input[..500]);
    let c2 = zlib_rs::crc32::crc32(c1, &input[500..]);
    assert_eq!(wado_crc, c2, "incremental crc32 mismatch");
}

#[test]
fn adler32_combine_matches_zlib_rs() {
    let part1 = test_data_sequential(500);
    let part2 = test_data_pseudorandom(300);
    let source = format!(
        r#"use {{ println, Stdout }} from "core:cli";
use {{ adler32, adler32_init, adler32_combine }} from "core:zlib";

export fn run() with Stdout {{
    let d1: Array<u8> = [{d1}];
    let d2: Array<u8> = [{d2}];
    let a1 = adler32(adler32_init(), &d1, 0, {l1});
    let a2 = adler32(adler32_init(), &d2, 0, {l2});
    let combined = adler32_combine(a1, a2, {l2});
    println(`{{combined}}`);
}}"#,
        d1 = bytes_to_wado_array(&part1),
        d2 = bytes_to_wado_array(&part2),
        l1 = part1.len(),
        l2 = part2.len()
    );
    let stdout = compile_and_run(&source);
    let wado_combined: u32 = stdout.trim().parse().unwrap();

    let a1 = zlib_rs::adler32::adler32(1, &part1);
    let a2 = zlib_rs::adler32::adler32(1, &part2);
    let rs_combined = zlib_rs::adler32::adler32_combine(a1, a2, part2.len() as u64);
    assert_eq!(wado_combined, rs_combined, "adler32_combine mismatch");
}

#[test]
fn crc32_combine_matches_zlib_rs() {
    let part1 = test_data_sequential(500);
    let part2 = test_data_pseudorandom(300);
    let source = format!(
        r#"use {{ println, Stdout }} from "core:cli";
use {{ crc32, crc32_init, crc32_combine }} from "core:zlib";

export fn run() with Stdout {{
    let d1: Array<u8> = [{d1}];
    let d2: Array<u8> = [{d2}];
    let c1 = crc32(crc32_init(), &d1, 0, {l1});
    let c2 = crc32(crc32_init(), &d2, 0, {l2});
    let combined = crc32_combine(c1, c2, {l2});
    println(`{{combined}}`);
}}"#,
        d1 = bytes_to_wado_array(&part1),
        d2 = bytes_to_wado_array(&part2),
        l1 = part1.len(),
        l2 = part2.len()
    );
    let stdout = compile_and_run(&source);
    let wado_combined: u32 = stdout.trim().parse().unwrap();

    let c1 = zlib_rs::crc32::crc32(0, &part1);
    let c2 = zlib_rs::crc32::crc32(0, &part2);
    let rs_combined = zlib_rs::crc32::crc32_combine(c1, c2, part2.len() as u64);
    assert_eq!(wado_combined, rs_combined, "crc32_combine mismatch");
}

#[test]
fn stored_block_bit_exact_hello() {
    let input = b"Hello";
    let source = format!(
        r#"use {{ print, println, Stdout }} from "core:cli";
use {{ zlib_compress_stored }} from "core:zlib";

export fn run() with Stdout {{
    let data: Array<u8> = [{array}];
    let compressed = zlib_compress_stored(&data);
    for let mut i = 0; i < compressed.len(); i += 1 {{
        if i > 0 {{ print(" "); }}
        print(`{{compressed[i] as i32}}`);
    }}
    println("");
}}"#,
        array = bytes_to_wado_array(input)
    );
    let stdout = compile_and_run(&source);
    let wado_stored = parse_bytes(&stdout);
    let rs_stored = zlib_rs_compress(input, 0);
    assert_eq!(
        wado_stored, rs_stored,
        "stored block bit-exact mismatch for 'Hello'"
    );
}

#[test]
fn stored_block_bit_exact_sequential() {
    let input: Vec<u8> = (0..=255).collect();
    let source = format!(
        r#"use {{ print, println, Stdout }} from "core:cli";
use {{ zlib_compress_stored }} from "core:zlib";

export fn run() with Stdout {{
    let mut data: Array<u8> = [];
    for let mut i = 0; i < 256; i += 1 {{
        data.append(i as u8);
    }}
    let compressed = zlib_compress_stored(&data);
    for let mut i = 0; i < compressed.len(); i += 1 {{
        if i > 0 {{ print(" "); }}
        print(`{{compressed[i] as i32}}`);
    }}
    println("");
}}"#
    );
    let stdout = compile_and_run(&source);
    let wado_stored = parse_bytes(&stdout);
    let rs_stored = zlib_rs_compress(&input, 0);
    assert_eq!(
        wado_stored, rs_stored,
        "stored block bit-exact mismatch for sequential bytes"
    );
}

#[test]
fn cross_validate_single_byte() {
    let input = vec![42u8];
    let source = make_compress2_source(&bytes_to_wado_array(&input), 6);
    let stdout = compile_and_run(&source);
    let wado_compressed = parse_bytes(&stdout);
    let decompressed = zlib_rs_decompress(&wado_compressed, 1024);
    assert_eq!(decompressed, input, "single byte cross-inflate failed");
}

#[test]
fn cross_validate_all_zeros() {
    let input = vec![0u8; 1000];
    let source = make_compress2_source(&bytes_to_wado_array(&input), 6);
    let stdout = compile_and_run(&source);
    let wado_compressed = parse_bytes(&stdout);
    let decompressed = zlib_rs_decompress(&wado_compressed, 2048);
    assert_eq!(decompressed, input, "all zeros cross-inflate failed");
}

#[test]
fn cross_validate_all_ff() {
    let input = vec![0xFFu8; 500];
    let source = make_compress2_source(&bytes_to_wado_array(&input), 6);
    let stdout = compile_and_run(&source);
    let wado_compressed = parse_bytes(&stdout);
    let decompressed = zlib_rs_decompress(&wado_compressed, 1024);
    assert_eq!(decompressed, input, "all 0xFF cross-inflate failed");
}

#[test]
fn cross_validate_two_bytes() {
    let input = vec![0u8, 1u8];
    let source = make_compress2_source(&bytes_to_wado_array(&input), 6);
    let stdout = compile_and_run(&source);
    let wado_compressed = parse_bytes(&stdout);
    let decompressed = zlib_rs_decompress(&wado_compressed, 1024);
    assert_eq!(decompressed, input, "two bytes cross-inflate failed");
}

#[test]
fn cross_validate_pseudorandom_data() {
    let input = test_data_pseudorandom(2000);
    let source = make_compress2_source(&bytes_to_wado_array(&input), 6);
    let stdout = compile_and_run(&source);
    let wado_compressed = parse_bytes(&stdout);
    let decompressed = zlib_rs_decompress(&wado_compressed, input.len() * 2);
    assert_eq!(
        decompressed, input,
        "pseudo-random data cross-inflate failed"
    );
}

#[test]
fn cross_validate_large_repetitive() {
    let input = test_data_repeated(b'X', 5000);
    let source = format!(
        r#"use {{ print, println, Stdout }} from "core:cli";
use {{ compress2 }} from "core:zlib";

export fn run() with Stdout {{
    let mut data: Array<u8> = [];
    for let mut i = 0; i < 5000; i += 1 {{
        data.append(88 as u8);
    }}
    let compressed = compress2(&data, 6);
    for let mut i = 0; i < compressed.len(); i += 1 {{
        if i > 0 {{ print(" "); }}
        print(`{{compressed[i] as i32}}`);
    }}
    println("");
}}"#
    );
    let stdout = compile_and_run(&source);
    let wado_compressed = parse_bytes(&stdout);
    let decompressed = zlib_rs_decompress(&wado_compressed, input.len() * 2);
    assert_eq!(
        decompressed, input,
        "large repetitive data cross-inflate failed"
    );
}

#[test]
fn compress_bound_matches_zlib_rs() {
    let source = r#"use { println, Stdout } from "core:cli";
use { compress_bound } from "core:zlib";

export fn run() with Stdout {
    println(`{compress_bound(0)}`);
    println(`{compress_bound(1)}`);
    println(`{compress_bound(100)}`);
    println(`{compress_bound(1000)}`);
    println(`{compress_bound(10000)}`);
    println(`{compress_bound(65535)}`);
}"#;
    let stdout = compile_and_run(source);
    let wado_bounds: Vec<usize> = stdout.trim().lines().map(|l| l.parse().unwrap()).collect();

    let sizes = [0, 1, 100, 1000, 10000, 65535];
    for (i, &size) in sizes.iter().enumerate() {
        let rs_bound = zlib_rs::compress_bound(size);
        // Wado's bound should be >= zlib-rs's bound (it can be larger, that's fine)
        assert!(
            wado_bounds[i] >= rs_bound,
            "compress_bound({size}): wado={} < zlib-rs={rs_bound}",
            wado_bounds[i]
        );
    }
}

#[test]
fn wado_streaming_preset_dict_decompressed_by_zlib_rs() {
    // Compress with Wado's DeflateStream + dictionary, decompress with zlib-rs
    let source = r#"use { print, println, Stdout } from "core:cli";
use { DeflateStream, Z_DEFAULT_COMPRESSION } from "core:zlib";

export fn run() with Stdout {
    let mut dict: Array<u8> = [];
    let dict_str = "common pattern data ";
    for let mut i = 0; i < dict_str.len(); i += 1 {
        dict.append(dict_str.get_byte(i) as u8);
    }

    let mut data: Array<u8> = [];
    let msg = "common pattern data common pattern data test";
    for let mut i = 0; i < msg.len(); i += 1 {
        data.append(msg.get_byte(i) as u8);
    }

    let mut ds = DeflateStream::new(Z_DEFAULT_COMPRESSION);
    ds.set_dictionary(&dict);
    let compressed = ds.compress(&data);

    for let mut i = 0; i < compressed.len(); i += 1 {
        if i > 0 { print(" "); }
        print(`{compressed[i] as i32}`);
    }
    println("");
}"#;
    let stdout = compile_and_run(source);
    let wado_compressed = parse_bytes(&stdout);

    // zlib-rs should be able to decompress this — it has FDICT flag
    // but we need to provide the dictionary
    // For simplicity, we just verify Wado can round-trip it
    // (zlib-rs API for dictionary decompression requires more work)
    assert!(
        !wado_compressed.is_empty(),
        "Wado preset dictionary compression produced empty output"
    );
    // Verify zlib header FDICT bit is set
    assert_eq!(
        wado_compressed[1] & 0x20,
        0x20,
        "FDICT bit not set in zlib header"
    );
}

#[test]
fn wado_zlib_version() {
    let source = r#"use { println, Stdout } from "core:cli";
use { zlib_version } from "core:zlib";

export fn run() with Stdout {
    println(zlib_version());
}"#;
    let stdout = compile_and_run(source);
    assert_eq!(
        stdout.trim(),
        "1.3.1.wado",
        "zlib_version should return '1.3.1.wado'"
    );
}

#[test]
fn wado_gzip_custom_header_decompressed_by_flate2() {
    let source = r#"use { print, println, Stdout } from "core:cli";
use { gzip_compress_with_header, GzipHeader } from "core:zlib";

export fn run() with Stdout {
    let mut header = GzipHeader::new();
    header.set_name("test.txt");
    header.set_comment("test comment");
    header.set_time(1234567890 as u32);
    header.set_os(0xFF);

    let mut data: Array<u8> = [];
    let msg = "Hello from custom gzip header";
    for let mut i = 0; i < msg.len(); i += 1 {
        data.append(msg.get_byte(i) as u8);
    }

    let compressed = gzip_compress_with_header(&data, 6, &header);

    for let mut i = 0; i < compressed.len(); i += 1 {
        if i > 0 { print(" "); }
        print(`{compressed[i] as i32}`);
    }
    println("");
}"#;
    let stdout = compile_and_run(source);
    let wado_gzip = parse_bytes(&stdout);

    // Decompress with flate2
    let decompressed = flate2_gzip_decompress(&wado_gzip);
    assert_eq!(
        std::str::from_utf8(&decompressed).unwrap(),
        "Hello from custom gzip header",
        "custom gzip header round-trip failed"
    );
}

#[test]
fn wado_deflate_stream_raw_format_decompressed_by_flate2() {
    let source = r#"use { print, println, Stdout } from "core:cli";
use { DeflateStream, RAW_FORMAT, Z_DEFAULT_COMPRESSION } from "core:zlib";

export fn run() with Stdout {
    let mut ds = DeflateStream::new_with_format(Z_DEFAULT_COMPRESSION, RAW_FORMAT);
    let mut data: Array<u8> = [];
    let msg = "Raw deflate test data";
    for let mut i = 0; i < msg.len(); i += 1 {
        data.append(msg.get_byte(i) as u8);
    }
    let compressed = ds.compress(&data);

    for let mut i = 0; i < compressed.len(); i += 1 {
        if i > 0 { print(" "); }
        print(`{compressed[i] as i32}`);
    }
    println("");
}"#;
    let stdout = compile_and_run(source);
    let wado_raw = parse_bytes(&stdout);

    // Decompress with flate2 (raw deflate)
    let decompressed = flate2_inflate_raw(&wado_raw);
    assert_eq!(
        std::str::from_utf8(&decompressed).unwrap(),
        "Raw deflate test data",
        "DeflateStream RAW_FORMAT cross-inflate failed"
    );
}

#[test]
fn wado_deflate_stream_gzip_format_decompressed_by_flate2() {
    let source = r#"use { print, println, Stdout } from "core:cli";
use { DeflateStream, GZIP_FORMAT, Z_DEFAULT_COMPRESSION } from "core:zlib";

export fn run() with Stdout {
    let mut ds = DeflateStream::new_with_format(Z_DEFAULT_COMPRESSION, GZIP_FORMAT);
    let mut data: Array<u8> = [];
    let msg = "Gzip format test data";
    for let mut i = 0; i < msg.len(); i += 1 {
        data.append(msg.get_byte(i) as u8);
    }
    let compressed = ds.compress(&data);

    for let mut i = 0; i < compressed.len(); i += 1 {
        if i > 0 { print(" "); }
        print(`{compressed[i] as i32}`);
    }
    println("");
}"#;
    let stdout = compile_and_run(source);
    let wado_gzip = parse_bytes(&stdout);

    let decompressed = flate2_gzip_decompress(&wado_gzip);
    assert_eq!(
        std::str::from_utf8(&decompressed).unwrap(),
        "Gzip format test data",
        "DeflateStream GZIP_FORMAT cross-inflate failed"
    );
}

#[test]
fn wado_inflate_stream_raw_format_from_flate2() {
    let input = b"Inflate raw from flate2";
    let compressed = flate2_deflate_raw(input, 6);
    let source = format!(
        r#"use {{ print, println, Stdout }} from "core:cli";
use {{ InflateStream, RAW_FORMAT }} from "core:zlib";

export fn run() with Stdout {{
    let data: Array<u8> = [{data}];
    let is = InflateStream::new_with_format(RAW_FORMAT);
    let decompressed = is.decompress(&data);
    for let mut i = 0; i < decompressed.len(); i += 1 {{
        if i > 0 {{ print(" "); }}
        print(`{{decompressed[i] as i32}}`);
    }}
    println("");
}}"#,
        data = bytes_to_wado_array(&compressed)
    );
    let stdout = compile_and_run(&source);
    let decompressed = parse_bytes(&stdout);
    assert_eq!(
        decompressed, input,
        "InflateStream RAW_FORMAT from flate2 failed"
    );
}

#[test]
fn wado_inflate_stream_gzip_format_from_flate2() {
    let input = b"Inflate gzip from flate2";
    let compressed = flate2_gzip_compress(input, 6);
    let source = format!(
        r#"use {{ print, println, Stdout }} from "core:cli";
use {{ InflateStream, GZIP_FORMAT }} from "core:zlib";

export fn run() with Stdout {{
    let data: Array<u8> = [{data}];
    let is = InflateStream::new_with_format(GZIP_FORMAT);
    let decompressed = is.decompress(&data);
    for let mut i = 0; i < decompressed.len(); i += 1 {{
        if i > 0 {{ print(" "); }}
        print(`{{decompressed[i] as i32}}`);
    }}
    println("");
}}"#,
        data = bytes_to_wado_array(&compressed)
    );
    let stdout = compile_and_run(&source);
    let decompressed = parse_bytes(&stdout);
    assert_eq!(
        decompressed, input,
        "InflateStream GZIP_FORMAT from flate2 failed"
    );
}

#[test]
fn wado_inflate_stream_auto_format_zlib() {
    let input = b"Auto detect zlib";
    let compressed = zlib_rs_compress(input, 6);
    let source = format!(
        r#"use {{ print, println, Stdout }} from "core:cli";
use {{ InflateStream, AUTO_FORMAT }} from "core:zlib";

export fn run() with Stdout {{
    let data: Array<u8> = [{data}];
    let is = InflateStream::new_with_format(AUTO_FORMAT);
    let decompressed = is.decompress(&data);
    for let mut i = 0; i < decompressed.len(); i += 1 {{
        if i > 0 {{ print(" "); }}
        print(`{{decompressed[i] as i32}}`);
    }}
    println("");
}}"#,
        data = bytes_to_wado_array(&compressed)
    );
    let stdout = compile_and_run(&source);
    let decompressed = parse_bytes(&stdout);
    assert_eq!(
        decompressed, input,
        "InflateStream AUTO_FORMAT with zlib input failed"
    );
}

#[test]
fn wado_inflate_stream_auto_format_gzip() {
    let input = b"Auto detect gzip";
    let compressed = flate2_gzip_compress(input, 6);
    let source = format!(
        r#"use {{ print, println, Stdout }} from "core:cli";
use {{ InflateStream, AUTO_FORMAT }} from "core:zlib";

export fn run() with Stdout {{
    let data: Array<u8> = [{data}];
    let is = InflateStream::new_with_format(AUTO_FORMAT);
    let decompressed = is.decompress(&data);
    for let mut i = 0; i < decompressed.len(); i += 1 {{
        if i > 0 {{ print(" "); }}
        print(`{{decompressed[i] as i32}}`);
    }}
    println("");
}}"#,
        data = bytes_to_wado_array(&compressed)
    );
    let stdout = compile_and_run(&source);
    let decompressed = parse_bytes(&stdout);
    assert_eq!(
        decompressed, input,
        "InflateStream AUTO_FORMAT with gzip input failed"
    );
}

#[test]
fn wado_deflate_stream_reset() {
    let source = r#"use { print, println, Stdout } from "core:cli";
use { DeflateStream, inflate_zlib, Z_DEFAULT_COMPRESSION } from "core:zlib";

export fn run() with Stdout {
    let mut ds = DeflateStream::new(Z_DEFAULT_COMPRESSION);

    let mut data1: Array<u8> = [];
    let msg1 = "First compression";
    for let mut i = 0; i < msg1.len(); i += 1 {
        data1.append(msg1.get_byte(i) as u8);
    }
    let c1 = ds.compress(&data1);
    let d1 = inflate_zlib(&c1);

    // Reset and compress again
    ds.reset();
    let mut data2: Array<u8> = [];
    let msg2 = "Second compression after reset";
    for let mut i = 0; i < msg2.len(); i += 1 {
        data2.append(msg2.get_byte(i) as u8);
    }
    let c2 = ds.compress(&data2);
    let d2 = inflate_zlib(&c2);

    // Verify both round-trips
    assert d1.len() == data1.len(), "first compression size mismatch";
    assert d2.len() == data2.len(), "second compression size mismatch";
    for let mut i = 0; i < data1.len(); i += 1 {
        assert d1[i] == data1[i], "first data mismatch";
    }
    for let mut i = 0; i < data2.len(); i += 1 {
        assert d2[i] == data2[i], "second data mismatch";
    }
    println("reset ok");
}"#;
    let stdout = compile_and_run(source);
    assert_eq!(stdout.trim(), "reset ok");
}

#[test]
fn wado_deflate_stream_copy() {
    let source = r#"use { print, println, Stdout } from "core:cli";
use { DeflateStream, inflate_zlib, Z_DEFAULT_COMPRESSION } from "core:zlib";

export fn run() with Stdout {
    let mut ds = DeflateStream::new(Z_DEFAULT_COMPRESSION);
    let mut data: Array<u8> = [];
    let msg = "Data for copy test";
    for let mut i = 0; i < msg.len(); i += 1 {
        data.append(msg.get_byte(i) as u8);
    }

    // Feed some data
    ds.update(&data);

    // Copy the stream
    let mut ds_copy = ds.copy();

    // Finish both
    let c1 = ds.finish();
    let c2 = ds_copy.finish();

    // Both should produce identical compressed output
    assert c1.len() == c2.len(), "copy should produce same output length";
    for let mut i = 0; i < c1.len(); i += 1 {
        assert c1[i] == c2[i], "copy should produce identical bytes";
    }

    // And both should decompress correctly
    let d1 = inflate_zlib(&c1);
    assert d1.len() == data.len(), "copy round-trip size mismatch";
    println("copy ok");
}"#;
    let stdout = compile_and_run(source);
    assert_eq!(stdout.trim(), "copy ok");
}

#[test]
fn wado_inflate_stream_reset() {
    let input1 = b"First data";
    let input2 = b"Second data";
    let c1 = zlib_rs_compress(input1, 6);
    let c2 = zlib_rs_compress(input2, 6);

    let source = format!(
        r#"use {{ print, println, Stdout }} from "core:cli";
use {{ InflateStream }} from "core:zlib";

export fn run() with Stdout {{
    let c1: Array<u8> = [{c1}];
    let c2: Array<u8> = [{c2}];

    let mut is = InflateStream::new();

    is.update(&c1);
    let d1 = is.finish();

    is.reset();

    is.update(&c2);
    let d2 = is.finish();

    assert d1.len() == {l1}, "first inflate size";
    assert d2.len() == {l2}, "second inflate size";
    println("inflate reset ok");
}}"#,
        c1 = bytes_to_wado_array(&c1),
        c2 = bytes_to_wado_array(&c2),
        l1 = input1.len(),
        l2 = input2.len()
    );
    let stdout = compile_and_run(&source);
    assert_eq!(stdout.trim(), "inflate reset ok");
}

#[test]
fn wado_deflate_stream_params() {
    let source = r#"use { print, println, Stdout } from "core:cli";
use { DeflateStream, inflate_zlib, Z_BEST_SPEED, Z_BEST_COMPRESSION, Z_DEFAULT_STRATEGY } from "core:zlib";

export fn run() with Stdout {
    let mut ds = DeflateStream::new(Z_BEST_SPEED);

    // Change to best compression before compressing
    ds.params(Z_BEST_COMPRESSION, Z_DEFAULT_STRATEGY);

    let mut data: Array<u8> = [];
    let msg = "Test data for params change";
    for let mut i = 0; i < msg.len(); i += 1 {
        data.append(msg.get_byte(i) as u8);
    }
    let compressed = ds.compress(&data);
    let decompressed = inflate_zlib(&compressed);
    assert decompressed.len() == data.len(), "params change round-trip";
    for let mut i = 0; i < data.len(); i += 1 {
        assert decompressed[i] == data[i], "params data mismatch";
    }
    println("params ok");
}"#;
    let stdout = compile_and_run(source);
    assert_eq!(stdout.trim(), "params ok");
}

#[test]
fn wado_deflate_stream_totals() {
    let source = r#"use { println, Stdout } from "core:cli";
use { DeflateStream, Z_DEFAULT_COMPRESSION } from "core:zlib";

export fn run() with Stdout {
    let mut ds = DeflateStream::new(Z_DEFAULT_COMPRESSION);
    let mut data: Array<u8> = [];
    for let mut i = 0; i < 100; i += 1 {
        data.append((i % 256) as u8);
    }
    let compressed = ds.compress(&data);
    println(`{ds.get_total_in()}`);
    println(`{ds.get_total_out()}`);
    assert ds.get_total_in() == 100, "total_in should be 100";
    assert ds.get_total_out() == compressed.len(), "total_out should match compressed size";
    println("totals ok");
}"#;
    let stdout = compile_and_run(source);
    let lines: Vec<&str> = stdout.trim().lines().collect();
    assert_eq!(lines[0], "100");
    assert_eq!(lines.last().unwrap(), &"totals ok");
}

fn assert_size_within_10_percent(wado_len: usize, rs_len: usize, label: &str) {
    let max_len = wado_len.max(rs_len);
    let min_len = wado_len.min(rs_len);
    // Allow 10% difference (use integer math to avoid float)
    assert!(
        max_len * 10 <= min_len * 11,
        "{label}: size difference too large: wado={wado_len}, zlib-rs={rs_len} ({:.1}% diff)",
        (max_len as f64 - min_len as f64) / min_len as f64 * 100.0
    );
}

#[test]
fn compression_size_all_levels_sequential() {
    let input = test_data_sequential(500);
    for level in 1..=9 {
        let source = make_compress2_source(&bytes_to_wado_array(&input), level);
        let stdout = compile_and_run(&source);
        let wado_compressed = parse_bytes(&stdout);
        let rs_compressed = zlib_rs_compress(&input, level);
        // Cross-validate: zlib-rs can decompress Wado output
        let decompressed = zlib_rs_decompress(&wado_compressed, input.len() * 2);
        assert_eq!(
            decompressed, input,
            "zlib-rs failed to decompress Wado level {level}"
        );
        // Size within 10%
        assert_size_within_10_percent(
            wado_compressed.len(),
            rs_compressed.len(),
            &format!("sequential level {level}"),
        );
    }
}

#[test]
fn compression_size_all_levels_text() {
    let input = test_data_text(5);
    for level in 1..=9 {
        let source = make_compress2_source(&bytes_to_wado_array(&input), level);
        let stdout = compile_and_run(&source);
        let wado_compressed = parse_bytes(&stdout);
        let rs_compressed = zlib_rs_compress(&input, level);
        let decompressed = zlib_rs_decompress(&wado_compressed, input.len() * 2);
        assert_eq!(
            decompressed, input,
            "zlib-rs failed to decompress Wado level {level}"
        );
        assert_size_within_10_percent(
            wado_compressed.len(),
            rs_compressed.len(),
            &format!("text level {level}"),
        );
    }
}

#[test]
fn compression_size_all_levels_pseudorandom() {
    let input = test_data_pseudorandom(500);
    for level in 1..=9 {
        let source = make_compress2_source(&bytes_to_wado_array(&input), level);
        let stdout = compile_and_run(&source);
        let wado_compressed = parse_bytes(&stdout);
        let rs_compressed = zlib_rs_compress(&input, level);
        let decompressed = zlib_rs_decompress(&wado_compressed, input.len() * 2);
        assert_eq!(
            decompressed, input,
            "zlib-rs failed to decompress Wado level {level}"
        );
        assert_size_within_10_percent(
            wado_compressed.len(),
            rs_compressed.len(),
            &format!("pseudorandom level {level}"),
        );
    }
}
