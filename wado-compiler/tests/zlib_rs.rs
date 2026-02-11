//! Bit-exact tests comparing Wado's core:zlib implementation against zlib-rs
//!
//! These tests verify:
//! - Wado's zlib_compress output can be decompressed by zlib-rs (cross-inflate)
//! - zlib-rs compressed output can be decompressed by Wado's inflate_zlib (cross-inflate)
//! - Inflate round-trip: inflate(deflate(data)) == data for both implementations
//! - Checksum compatibility (adler32, crc32) between Wado and zlib-rs

mod common;

use zlib_rs::{DeflateConfig, InflateConfig, ReturnCode};

// ============================================================================
// Helpers
// ============================================================================

/// Build Wado source that compresses data (given as array literal) and outputs
/// compressed bytes as space-separated decimal integers.
fn make_compress_source(array_literal: &str) -> String {
    format!(
        r#"use {{ print, println, Stdout }} from "core:cli";
use {{ zlib_compress }} from "core:zlib";

export fn run() with Stdout {{
    let data: Array<u8> = [{array_literal}];
    let compressed = zlib_compress(&data);
    for let mut i = 0; i < compressed.len(); i += 1 {{
        if i > 0 {{ print(" "); }}
        print(`{{compressed[i] as i32}}`);
    }}
    println("");
}}"#
    )
}

/// Build Wado source that compresses repetitive data (count copies of byte_val)
/// and outputs compressed bytes as space-separated decimal integers.
fn make_compress_repetitive_source(byte_val: u8, count: usize) -> String {
    format!(
        r#"use {{ print, println, Stdout }} from "core:cli";
use {{ zlib_compress }} from "core:zlib";

export fn run() with Stdout {{
    let mut data: Array<u8> = [];
    for let mut i = 0; i < {count}; i += 1 {{
        data.append({byte_val} as u8);
    }}
    let compressed = zlib_compress(&data);
    for let mut i = 0; i < compressed.len(); i += 1 {{
        if i > 0 {{ print(" "); }}
        print(`{{compressed[i] as i32}}`);
    }}
    println("");
}}"#
    )
}

/// Build Wado source that decompresses zlib data (given as array literal) and
/// outputs decompressed bytes as space-separated decimal integers.
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

/// Build Wado source that computes adler32 and crc32 checksums for the given
/// data and prints them as "adler32 crc32".
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

/// Format a byte slice as Wado array literal elements: "72 as u8, 101 as u8, ..."
fn bytes_to_wado_array(data: &[u8]) -> String {
    data.iter()
        .map(|b| format!("{b} as u8"))
        .collect::<Vec<_>>()
        .join(", ")
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

/// Compress data using zlib-rs
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

/// Compile Wado source and run it, returning stdout
fn compile_and_run(source: &str) -> String {
    let result = common::compile_source(source).expect("compilation failed");
    let run_result = common::run_wasm(result.wasm).expect("wasm execution failed");
    assert!(!run_result.trapped, "Wasm trapped: {}", run_result.stderr);
    run_result.stdout
}

// ============================================================================
// Tests: Wado compress -> zlib-rs decompress
// ============================================================================

#[test]
fn wado_compress_hello_decompressed_by_zlib_rs() {
    let input = b"Hello";
    let source = make_compress_source(&bytes_to_wado_array(input));
    let stdout = compile_and_run(&source);
    let wado_compressed = parse_bytes(&stdout);

    let decompressed = zlib_rs_decompress(&wado_compressed, 1024);
    assert_eq!(
        decompressed, input,
        "zlib-rs failed to decompress Wado-compressed 'Hello'"
    );
}

#[test]
fn wado_compress_repetitive_decompressed_by_zlib_rs() {
    let input: Vec<u8> = vec![b'A'; 1000];
    let source = make_compress_repetitive_source(b'A', 1000);
    let stdout = compile_and_run(&source);
    let wado_compressed = parse_bytes(&stdout);

    let decompressed = zlib_rs_decompress(&wado_compressed, 2048);
    assert_eq!(
        decompressed, input,
        "zlib-rs failed to decompress Wado-compressed 1000x'A'"
    );
}

#[test]
fn wado_compress_empty_decompressed_by_zlib_rs() {
    let source = make_compress_source("");
    let stdout = compile_and_run(&source);
    let wado_compressed = parse_bytes(&stdout);

    let decompressed = zlib_rs_decompress(&wado_compressed, 1024);
    assert!(
        decompressed.is_empty(),
        "zlib-rs decompressed non-empty from Wado-compressed empty"
    );
}

#[test]
fn wado_compress_hello_world_x20_decompressed_by_zlib_rs() {
    let input: Vec<u8> = b"Hello, World! ".repeat(20);
    let source = format!(
        r#"use {{ print, println, Stdout }} from "core:cli";
use {{ zlib_compress }} from "core:zlib";

export fn run() with Stdout {{
    let mut data: Array<u8> = [];
    let hw: Array<u8> = [{hw}];
    for let mut j = 0; j < 20; j += 1 {{
        for let mut i = 0; i < 14; i += 1 {{
            data.append(hw[i]);
        }}
    }}
    let compressed = zlib_compress(&data);
    for let mut i = 0; i < compressed.len(); i += 1 {{
        if i > 0 {{ print(" "); }}
        print(`{{compressed[i] as i32}}`);
    }}
    println("");
}}"#,
        hw = bytes_to_wado_array(b"Hello, World! ")
    );
    let stdout = compile_and_run(&source);
    let wado_compressed = parse_bytes(&stdout);

    let decompressed = zlib_rs_decompress(&wado_compressed, 1024);
    assert_eq!(
        decompressed, input,
        "zlib-rs failed to decompress Wado-compressed 'Hello, World! ' x20"
    );
}

// ============================================================================
// Tests: zlib-rs compress -> Wado decompress
// ============================================================================

#[test]
fn zlib_rs_compressed_hello_inflated_by_wado() {
    let input = b"Hello";
    let compressed = zlib_rs_compress(input, 6);

    let source = make_inflate_source(&bytes_to_wado_array(&compressed));
    let stdout = compile_and_run(&source);
    let decompressed = parse_bytes(&stdout);

    assert_eq!(
        decompressed, input,
        "Wado failed to inflate zlib-rs-compressed 'Hello'"
    );
}

#[test]
fn zlib_rs_compressed_repetitive_inflated_by_wado() {
    let input: Vec<u8> = vec![b'A'; 1000];
    let compressed = zlib_rs_compress(&input, 6);

    let source = make_inflate_source(&bytes_to_wado_array(&compressed));
    let stdout = compile_and_run(&source);
    let decompressed = parse_bytes(&stdout);

    assert_eq!(
        decompressed, input,
        "Wado failed to inflate zlib-rs-compressed 1000x'A'"
    );
}

#[test]
fn zlib_rs_compressed_bytes_0_to_255_inflated_by_wado() {
    let input: Vec<u8> = (0..=255).collect();
    let compressed = zlib_rs_compress(&input, 6);

    let source = make_inflate_source(&bytes_to_wado_array(&compressed));
    let stdout = compile_and_run(&source);
    let decompressed = parse_bytes(&stdout);

    assert_eq!(
        decompressed, input,
        "Wado failed to inflate zlib-rs-compressed bytes 0..=255"
    );
}

#[test]
fn zlib_rs_compressed_empty_inflated_by_wado() {
    let input = b"";
    let compressed = zlib_rs_compress(input, 6);

    let source = make_inflate_source(&bytes_to_wado_array(&compressed));
    let stdout = compile_and_run(&source);
    let decompressed = parse_bytes(&stdout);

    assert!(
        decompressed.is_empty(),
        "Wado inflated non-empty from zlib-rs-compressed empty"
    );
}

// ============================================================================
// Tests: Round-trip through both implementations
// ============================================================================

#[test]
fn round_trip_wado_compress_zlib_rs_decompress_all_byte_values() {
    let input: Vec<u8> = (0..=255).collect();
    let source = format!(
        r#"use {{ print, println, Stdout }} from "core:cli";
use {{ zlib_compress }} from "core:zlib";

export fn run() with Stdout {{
    let mut data: Array<u8> = [];
    for let mut i = 0; i < 256; i += 1 {{
        data.append(i as u8);
    }}
    let compressed = zlib_compress(&data);
    for let mut i = 0; i < compressed.len(); i += 1 {{
        if i > 0 {{ print(" "); }}
        print(`{{compressed[i] as i32}}`);
    }}
    println("");
}}"#
    );
    let stdout = compile_and_run(&source);
    let wado_compressed = parse_bytes(&stdout);

    let decompressed = zlib_rs_decompress(&wado_compressed, 1024);
    assert_eq!(
        decompressed, input,
        "Round-trip failed for bytes 0..=255"
    );
}

// ============================================================================
// Tests: Checksum compatibility
// ============================================================================

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

    assert_eq!(
        wado_adler, rs_adler,
        "adler32 mismatch for 'Hello': wado={wado_adler}, zlib-rs={rs_adler}"
    );
    assert_eq!(
        wado_crc, rs_crc,
        "crc32 mismatch for 'Hello': wado={wado_crc}, zlib-rs={rs_crc}"
    );
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

    assert_eq!(
        wado_adler, rs_adler,
        "adler32 mismatch for bytes 0..=255"
    );
    assert_eq!(
        wado_crc, rs_crc,
        "crc32 mismatch for bytes 0..=255"
    );
}

#[test]
fn adler32_crc32_empty_matches_zlib_rs() {
    let input = b"";
    let source = make_checksum_source("", 0);
    let stdout = compile_and_run(&source);

    let parts: Vec<&str> = stdout.trim().split(' ').collect();
    let wado_adler: u32 = parts[0].parse().unwrap();
    let wado_crc: u32 = parts[1].parse().unwrap();

    let rs_adler = zlib_rs::adler32::adler32(1, input);
    let rs_crc = zlib_rs::crc32::crc32(0, input);

    assert_eq!(
        wado_adler, rs_adler,
        "adler32 mismatch for empty input"
    );
    assert_eq!(
        wado_crc, rs_crc,
        "crc32 mismatch for empty input"
    );
}

// ============================================================================
// Tests: Bit-exact stored block comparison
// ============================================================================

#[test]
fn stored_block_bit_exact_hello() {
    // Wado's zlib_compress_stored should produce identical output to zlib-rs level=0
    // for small data, since stored blocks have only one valid encoding.
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
        "stored block bit-exact mismatch for 'Hello':\n  wado: {wado_stored:?}\n  zlib-rs: {rs_stored:?}"
    );
}
