//! Cross-validation tests comparing Wado's core:zlib implementation against
//! zlib-rs and flate2 (backed by zlib-rs).
//!
//! Architecture: Two Wado driver programs are compiled once and reused across
//! all tests. Each test only performs cheap Wasm instantiation (no compilation).
//! This eliminates the memory pressure from concurrent compilations that caused
//! SIGSEGV flakiness.
//!
//! Drivers:
//! - `zlib_cross_compress.wado`: compress/checksum operations
//! - `zlib_cross_inflate.wado`: inflate/decompress operations

mod common;

use flate2::Compression;
use flate2::write::{DeflateDecoder, DeflateEncoder, GzDecoder, GzEncoder};
use std::io::Write;
use std::path::Path;
use std::sync::OnceLock;
use wasmtime::Store;
use wasmtime::component::Component;
use wasmtime_wasi::p2::pipe::{MemoryInputPipe, MemoryOutputPipe};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder};
use zlib_rs::{DeflateConfig, InflateConfig, ReturnCode};

static COMPRESS_COMPONENT: OnceLock<Component> = OnceLock::new();
static INFLATE_COMPONENT: OnceLock<Component> = OnceLock::new();

fn compile_driver(fixture_name: &str) -> Component {
    let fixtures_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sub");
    let path = fixtures_dir.join(fixture_name);
    let result = common::compile_file(&path).unwrap_or_else(|e| {
        panic!("Failed to compile {fixture_name}: {e:?}");
    });
    let engine = common::cli_engine();
    Component::new(engine, &result.wasm)
        .unwrap_or_else(|e| panic!("Failed to create component for {fixture_name}: {e}"))
}

fn compress_component() -> &'static Component {
    COMPRESS_COMPONENT.get_or_init(|| compile_driver("zlib_cross_compress.wado"))
}

fn inflate_component() -> &'static Component {
    INFLATE_COMPONENT.get_or_init(|| compile_driver("zlib_cross_inflate.wado"))
}

fn run_component(component: &Component, stdin: &[u8]) -> String {
    let rt = common::runtime();
    let engine = common::cli_engine();

    rt.block_on(async {
        let linker = common::cli_linker(engine).expect("failed to create linker");

        let stdout_pipe = MemoryOutputPipe::new(1 << 20);
        let stdout_clone = stdout_pipe.clone();
        let stderr_pipe = MemoryOutputPipe::new(65536);
        let stderr_clone = stderr_pipe.clone();

        let ctx: WasiCtx = WasiCtxBuilder::new()
            .stdin(MemoryInputPipe::new(stdin.to_vec()))
            .stdout(stdout_pipe)
            .stderr(stderr_pipe)
            .build();
        common::install_rustls_provider_for_tests();
        let state = common::WasiState {
            ctx,
            table: wasmtime::component::ResourceTable::new(),
            http_ctx: wasmtime_wasi_http::WasiHttpCtx::new(),
            http_hooks: common::TestHttpCtx::new(),
            tls_ctx: wasmtime_wasi_tls::WasiTlsCtxBuilder::new().build(),
        };
        let mut store = Store::new(engine, state);
        // Set epoch deadline for timeout enforcement.
        // zlib tests with large data can take >5s, so use 30s timeout.
        let deadline_ticks = 30;
        store.set_epoch_deadline(deadline_ticks);

        let instance = linker
            .instantiate_async(&mut store, component)
            .await
            .expect("failed to instantiate");
        let run_func = instance
            .get_typed_func::<(), (Result<(), ()>,)>(&mut store, "run")
            .expect("failed to get run func");

        match run_func.call_async(&mut store, ()).await {
            Ok((result,)) => {
                if result.is_err() {
                    let stderr =
                        String::from_utf8(stderr_clone.contents().to_vec()).unwrap_or_default();
                    panic!("Wasm component returned error. stderr: {stderr}");
                }
            }
            Err(e) => {
                let stderr =
                    String::from_utf8(stderr_clone.contents().to_vec()).unwrap_or_default();
                panic!("Wasm component trapped: {e:#}\nstderr: {stderr}");
            }
        }

        String::from_utf8(stdout_clone.contents().to_vec()).expect("stdout not valid UTF-8")
    })
}

fn parse_bytes(stdout: &str) -> Vec<u8> {
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return vec![];
    }
    trimmed
        .split(' ')
        .map(|s| {
            s.parse::<u8>()
                .unwrap_or_else(|_| panic!("failed to parse byte: {s:?}"))
        })
        .collect()
}

fn be_u32(v: u32) -> [u8; 4] {
    v.to_be_bytes()
}

fn be_i32(v: i32) -> [u8; 4] {
    v.to_be_bytes()
}

fn wado_compress2(data: &[u8], level: i32) -> Vec<u8> {
    let mut stdin = vec![0u8, level as u8];
    stdin.extend_from_slice(data);
    parse_bytes(&run_component(compress_component(), &stdin))
}

fn wado_deflate_raw(data: &[u8], level: i32) -> Vec<u8> {
    let mut stdin = vec![1u8, level as u8];
    stdin.extend_from_slice(data);
    parse_bytes(&run_component(compress_component(), &stdin))
}

fn wado_gzip_compress(data: &[u8], level: i32) -> Vec<u8> {
    let mut stdin = vec![2u8, level as u8];
    stdin.extend_from_slice(data);
    parse_bytes(&run_component(compress_component(), &stdin))
}

fn wado_compress_stored(data: &[u8]) -> Vec<u8> {
    let mut stdin = vec![3u8];
    stdin.extend_from_slice(data);
    parse_bytes(&run_component(compress_component(), &stdin))
}

fn wado_compress_with_strategy(data: &[u8], level: i32, strategy: i32) -> Vec<u8> {
    let mut stdin = vec![4u8, level as u8, strategy as u8];
    stdin.extend_from_slice(data);
    parse_bytes(&run_component(compress_component(), &stdin))
}

fn wado_checksums(data: &[u8]) -> (u32, u32) {
    let mut stdin = vec![5u8];
    stdin.extend_from_slice(data);
    let stdout = run_component(compress_component(), &stdin);
    let parts: Vec<&str> = stdout.trim().split(' ').collect();
    (parts[0].parse().unwrap(), parts[1].parse().unwrap())
}

fn wado_compress_bound(size: i32) -> i32 {
    let mut stdin = vec![6u8];
    stdin.extend_from_slice(&be_i32(size));
    run_component(compress_component(), &stdin)
        .trim()
        .parse()
        .unwrap()
}

fn wado_version() -> String {
    let stdin = vec![7u8];
    run_component(compress_component(), &stdin)
        .trim()
        .to_string()
}

fn wado_adler32_combine(a1: u32, a2: u32, len2: i32) -> u32 {
    let mut stdin = vec![8u8];
    stdin.extend_from_slice(&be_u32(a1));
    stdin.extend_from_slice(&be_u32(a2));
    stdin.extend_from_slice(&be_i32(len2));
    run_component(compress_component(), &stdin)
        .trim()
        .parse()
        .unwrap()
}

fn wado_crc32_combine(c1: u32, c2: u32, len2: i32) -> u32 {
    let mut stdin = vec![9u8];
    stdin.extend_from_slice(&be_u32(c1));
    stdin.extend_from_slice(&be_u32(c2));
    stdin.extend_from_slice(&be_i32(len2));
    run_component(compress_component(), &stdin)
        .trim()
        .parse()
        .unwrap()
}

fn wado_adler32_incremental(data: &[u8], split: i32) -> u32 {
    let mut stdin = vec![10u8];
    stdin.extend_from_slice(&be_i32(split));
    stdin.extend_from_slice(data);
    run_component(compress_component(), &stdin)
        .trim()
        .parse()
        .unwrap()
}

fn wado_crc32_incremental(data: &[u8], split: i32) -> u32 {
    let mut stdin = vec![11u8];
    stdin.extend_from_slice(&be_i32(split));
    stdin.extend_from_slice(data);
    run_component(compress_component(), &stdin)
        .trim()
        .parse()
        .unwrap()
}

fn wado_deflate_stream_raw(data: &[u8], level: i32) -> Vec<u8> {
    let mut stdin = vec![13u8, level as u8];
    stdin.extend_from_slice(data);
    parse_bytes(&run_component(compress_component(), &stdin))
}

fn wado_deflate_stream_gzip(data: &[u8], level: i32) -> Vec<u8> {
    let mut stdin = vec![14u8, level as u8];
    stdin.extend_from_slice(data);
    parse_bytes(&run_component(compress_component(), &stdin))
}

fn wado_inflate_zlib(data: &[u8]) -> Vec<u8> {
    let mut stdin = vec![0u8];
    stdin.extend_from_slice(data);
    parse_bytes(&run_component(inflate_component(), &stdin))
}

fn wado_inflate_raw(data: &[u8]) -> Vec<u8> {
    let mut stdin = vec![1u8];
    stdin.extend_from_slice(data);
    parse_bytes(&run_component(inflate_component(), &stdin))
}

fn wado_inflate_gzip(data: &[u8]) -> Vec<u8> {
    let mut stdin = vec![2u8];
    stdin.extend_from_slice(data);
    parse_bytes(&run_component(inflate_component(), &stdin))
}

fn wado_inflate_stream_raw(data: &[u8]) -> Vec<u8> {
    let mut stdin = vec![3u8];
    stdin.extend_from_slice(data);
    parse_bytes(&run_component(inflate_component(), &stdin))
}

fn wado_inflate_stream_gzip(data: &[u8]) -> Vec<u8> {
    let mut stdin = vec![4u8];
    stdin.extend_from_slice(data);
    parse_bytes(&run_component(inflate_component(), &stdin))
}

fn wado_inflate_stream_auto(data: &[u8]) -> Vec<u8> {
    let mut stdin = vec![5u8];
    stdin.extend_from_slice(data);
    parse_bytes(&run_component(inflate_component(), &stdin))
}

fn zlib_rs_compress(input: &[u8], level: i32) -> Vec<u8> {
    let bound = zlib_rs::compress_bound(input.len());
    let mut output = vec![0u8; bound];
    let config = DeflateConfig::new(level);
    let (compressed, rc) = zlib_rs::compress_slice(&mut output, input, config);
    assert_eq!(rc, ReturnCode::Ok, "zlib-rs compress failed");
    compressed.to_vec()
}

fn zlib_rs_decompress(input: &[u8], max_output: usize) -> Vec<u8> {
    let mut output = vec![0u8; max_output];
    let (decompressed, rc) =
        zlib_rs::decompress_slice(&mut output, input, InflateConfig::default());
    assert_eq!(rc, ReturnCode::Ok, "zlib-rs decompress failed");
    decompressed.to_vec()
}

fn flate2_deflate_raw(input: &[u8], level: u32) -> Vec<u8> {
    let mut encoder = DeflateEncoder::new(Vec::new(), Compression::new(level));
    encoder.write_all(input).unwrap();
    encoder.finish().unwrap()
}

fn flate2_inflate_raw(input: &[u8]) -> Vec<u8> {
    let mut decoder = DeflateDecoder::new(Vec::new());
    decoder.write_all(input).unwrap();
    decoder.finish().unwrap()
}

fn flate2_gzip_compress(input: &[u8], level: u32) -> Vec<u8> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::new(level));
    encoder.write_all(input).unwrap();
    encoder.finish().unwrap()
}

fn flate2_gzip_decompress(input: &[u8]) -> Vec<u8> {
    let mut decoder = GzDecoder::new(Vec::new());
    decoder.write_all(input).unwrap();
    decoder.finish().unwrap()
}

fn test_data_sequential(size: usize) -> Vec<u8> {
    (0..size).map(|i| (i % 256) as u8).collect()
}

fn test_data_repeated(byte: u8, size: usize) -> Vec<u8> {
    vec![byte; size]
}

fn test_data_text(repeats: usize) -> Vec<u8> {
    b"Hello, World! This is a test of the zlib compression library. ".repeat(repeats)
}

fn date_seed() -> u32 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    // YYYYMMDD: same day → same seed
    let days = now / 86400;
    // Reconstruct YYYYMMDD from days since epoch
    // Simple civil-calendar conversion (good through 2099)
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y * 10000 + m * 100 + d) as u32
}

fn test_data_pseudorandom(size: usize) -> Vec<u8> {
    let mut data = Vec::with_capacity(size);
    let mut state: u32 = date_seed();
    for _ in 0..size {
        state = state.wrapping_mul(1_103_515_245).wrapping_add(12345);
        data.push((state >> 16) as u8);
    }
    data
}

// ============================================================================
// Wado compress → zlib-rs decompress
// ============================================================================

#[test]
fn wado_compress_hello_decompressed_by_zlib_rs() {
    let input = b"Hello";
    let compressed = wado_compress2(input, 6);
    let decompressed = zlib_rs_decompress(&compressed, 1024);
    assert_eq!(decompressed, input, "cross-inflate failed for 'Hello'");
}

#[test]
fn wado_compress_empty_decompressed_by_zlib_rs() {
    let compressed = wado_compress2(b"", 6);
    let decompressed = zlib_rs_decompress(&compressed, 1024);
    assert!(decompressed.is_empty(), "cross-inflate empty failed");
}

#[test]
fn wado_compress_all_levels_decompressed_by_zlib_rs() {
    let input = test_data_sequential(500);
    for level in 0..=9 {
        let compressed = wado_compress2(&input, level);
        let decompressed = zlib_rs_decompress(&compressed, input.len() * 2);
        assert_eq!(decompressed, input, "cross-inflate failed at level {level}");
    }
}

#[test]
fn wado_compress_strategy_filtered() {
    let input = test_data_sequential(500);
    let compressed = wado_compress_with_strategy(&input, 6, 1); // Z_FILTERED
    let decompressed = zlib_rs_decompress(&compressed, input.len() * 2);
    assert_eq!(decompressed, input, "Z_FILTERED cross-inflate failed");
}

#[test]
fn wado_compress_strategy_huffman_only() {
    let input = test_data_sequential(500);
    let compressed = wado_compress_with_strategy(&input, 6, 2); // Z_HUFFMAN_ONLY
    let decompressed = zlib_rs_decompress(&compressed, input.len() * 2);
    assert_eq!(decompressed, input, "Z_HUFFMAN_ONLY cross-inflate failed");
}

#[test]
fn wado_compress_strategy_rle() {
    let input = test_data_repeated(0x42, 500);
    let compressed = wado_compress_with_strategy(&input, 6, 3); // Z_RLE
    let decompressed = zlib_rs_decompress(&compressed, input.len() * 2);
    assert_eq!(decompressed, input, "Z_RLE cross-inflate failed");
}

#[test]
fn wado_compress_strategy_fixed() {
    let input = test_data_text(5);
    let compressed = wado_compress_with_strategy(&input, 6, 4); // Z_FIXED
    let decompressed = zlib_rs_decompress(&compressed, input.len() * 2);
    assert_eq!(decompressed, input, "Z_FIXED cross-inflate failed");
}

// ============================================================================
// zlib-rs compress → Wado inflate
// ============================================================================

#[test]
fn zlib_rs_all_levels_inflated_by_wado() {
    let input = test_data_sequential(500);
    for level in 0..=9 {
        let compressed = zlib_rs_compress(&input, level);
        let decompressed = wado_inflate_zlib(&compressed);
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
    let decompressed = wado_inflate_zlib(&compressed);
    assert_eq!(decompressed, input, "Wado inflate failed for text data");
}

#[test]
fn zlib_rs_pseudorandom_data_inflated_by_wado() {
    let input = test_data_pseudorandom(1000);
    let compressed = zlib_rs_compress(&input, 6);
    let decompressed = wado_inflate_zlib(&compressed);
    assert_eq!(
        decompressed, input,
        "Wado inflate failed for pseudo-random data"
    );
}

// ============================================================================
// Raw deflate cross-validation
// ============================================================================

#[test]
fn wado_raw_deflate_decompressed_by_flate2() {
    let input = test_data_text(5);
    let compressed = wado_deflate_raw(&input, 6);
    let decompressed = flate2_inflate_raw(&compressed);
    assert_eq!(
        decompressed, input,
        "flate2 failed to inflate Wado raw deflate"
    );
}

#[test]
fn flate2_raw_deflate_inflated_by_wado() {
    let input = test_data_sequential(300);
    let compressed = flate2_deflate_raw(&input, 6);
    let decompressed = wado_inflate_raw(&compressed);
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
        let decompressed = wado_inflate_raw(&compressed);
        assert_eq!(
            decompressed, input,
            "Wado inflate_raw failed for flate2 level {level}"
        );
    }
}

// ============================================================================
// Gzip cross-validation
// ============================================================================

#[test]
fn wado_gzip_compress_decompressed_by_flate2() {
    let input = test_data_text(5);
    let compressed = wado_gzip_compress(&input, 6);
    let decompressed = flate2_gzip_decompress(&compressed);
    assert_eq!(
        decompressed, input,
        "flate2 failed to decompress Wado gzip output"
    );
}

#[test]
fn flate2_gzip_inflated_by_wado() {
    let input = test_data_sequential(300);
    let compressed = flate2_gzip_compress(&input, 6);
    let decompressed = wado_inflate_gzip(&compressed);
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
        let decompressed = wado_inflate_gzip(&compressed);
        assert_eq!(
            decompressed, input,
            "Wado inflate_gzip failed for flate2 level {level}"
        );
    }
}

// ============================================================================
// Checksums
// ============================================================================

#[test]
fn adler32_crc32_hello_matches_zlib_rs() {
    let input = b"Hello";
    let (wado_adler, wado_crc) = wado_checksums(input);
    let rs_adler = zlib_rs::adler32::adler32(1, input);
    let rs_crc = zlib_rs::crc32::crc32(0, input);
    assert_eq!(wado_adler, rs_adler, "adler32 mismatch for 'Hello'");
    assert_eq!(wado_crc, rs_crc, "crc32 mismatch for 'Hello'");
}

#[test]
fn adler32_crc32_bytes_0_to_255_matches_zlib_rs() {
    let input: Vec<u8> = (0..=255).collect();
    let (wado_adler, wado_crc) = wado_checksums(&input);
    let rs_adler = zlib_rs::adler32::adler32(1, &input);
    let rs_crc = zlib_rs::crc32::crc32(0, &input);
    assert_eq!(wado_adler, rs_adler, "adler32 mismatch for bytes 0..=255");
    assert_eq!(wado_crc, rs_crc, "crc32 mismatch for bytes 0..=255");
}

#[test]
fn adler32_crc32_empty_matches_zlib_rs() {
    let (wado_adler, wado_crc) = wado_checksums(b"");
    let rs_adler = zlib_rs::adler32::adler32(1, b"");
    let rs_crc = zlib_rs::crc32::crc32(0, b"");
    assert_eq!(wado_adler, rs_adler, "adler32 mismatch for empty");
    assert_eq!(wado_crc, rs_crc, "crc32 mismatch for empty");
}

#[test]
fn adler32_crc32_large_data_matches_zlib_rs() {
    let input = test_data_pseudorandom(5000);
    let (wado_adler, wado_crc) = wado_checksums(&input);
    let rs_adler = zlib_rs::adler32::adler32(1, &input);
    let rs_crc = zlib_rs::crc32::crc32(0, &input);
    assert_eq!(wado_adler, rs_adler, "adler32 mismatch for large data");
    assert_eq!(wado_crc, rs_crc, "crc32 mismatch for large data");
}

#[test]
fn adler32_incremental_matches_zlib_rs() {
    let input = test_data_sequential(1000);
    let wado_adler = wado_adler32_incremental(&input, 500);
    let a1 = zlib_rs::adler32::adler32(1, &input[..500]);
    let a2 = zlib_rs::adler32::adler32(a1, &input[500..]);
    assert_eq!(wado_adler, a2, "incremental adler32 mismatch");
}

#[test]
fn crc32_incremental_matches_zlib_rs() {
    let input = test_data_sequential(1000);
    let wado_crc = wado_crc32_incremental(&input, 500);
    let c1 = zlib_rs::crc32::crc32(0, &input[..500]);
    let c2 = zlib_rs::crc32::crc32(c1, &input[500..]);
    assert_eq!(wado_crc, c2, "incremental crc32 mismatch");
}

#[test]
fn adler32_combine_matches_zlib_rs() {
    let part1 = test_data_sequential(500);
    let part2 = test_data_pseudorandom(300);
    let a1 = zlib_rs::adler32::adler32(1, &part1);
    let a2 = zlib_rs::adler32::adler32(1, &part2);
    let rs_combined = zlib_rs::adler32::adler32_combine(a1, a2, part2.len() as u64);
    let wado_combined = wado_adler32_combine(a1, a2, part2.len() as i32);
    assert_eq!(wado_combined, rs_combined, "adler32_combine mismatch");
}

#[test]
fn crc32_combine_matches_zlib_rs() {
    let part1 = test_data_sequential(500);
    let part2 = test_data_pseudorandom(300);
    let c1 = zlib_rs::crc32::crc32(0, &part1);
    let c2 = zlib_rs::crc32::crc32(0, &part2);
    let rs_combined = zlib_rs::crc32::crc32_combine(c1, c2, part2.len() as u64);
    let wado_combined = wado_crc32_combine(c1, c2, part2.len() as i32);
    assert_eq!(wado_combined, rs_combined, "crc32_combine mismatch");
}

// ============================================================================
// Stored blocks (bit-exact)
// ============================================================================

#[test]
fn stored_block_bit_exact_hello() {
    let input = b"Hello";
    let wado_stored = wado_compress_stored(input);
    let rs_stored = zlib_rs_compress(input, 0);
    assert_eq!(
        wado_stored, rs_stored,
        "stored block bit-exact mismatch for 'Hello'"
    );
}

#[test]
fn stored_block_bit_exact_sequential() {
    let input: Vec<u8> = (0..=255).collect();
    let wado_stored = wado_compress_stored(&input);
    let rs_stored = zlib_rs_compress(&input, 0);
    assert_eq!(
        wado_stored, rs_stored,
        "stored block bit-exact mismatch for sequential bytes"
    );
}

// ============================================================================
// Edge cases
// ============================================================================

#[test]
fn cross_validate_single_byte() {
    let input = vec![42u8];
    let compressed = wado_compress2(&input, 6);
    let decompressed = zlib_rs_decompress(&compressed, 1024);
    assert_eq!(decompressed, input, "single byte cross-inflate failed");
}

#[test]
fn cross_validate_all_zeros() {
    let input = vec![0u8; 1000];
    let compressed = wado_compress2(&input, 6);
    let decompressed = zlib_rs_decompress(&compressed, 2048);
    assert_eq!(decompressed, input, "all zeros cross-inflate failed");
}

#[test]
fn cross_validate_all_ff() {
    let input = vec![0xFFu8; 500];
    let compressed = wado_compress2(&input, 6);
    let decompressed = zlib_rs_decompress(&compressed, 1024);
    assert_eq!(decompressed, input, "all 0xFF cross-inflate failed");
}

#[test]
fn cross_validate_two_bytes() {
    let input = vec![0u8, 1u8];
    let compressed = wado_compress2(&input, 6);
    let decompressed = zlib_rs_decompress(&compressed, 1024);
    assert_eq!(decompressed, input, "two bytes cross-inflate failed");
}

#[test]
fn cross_validate_pseudorandom_data() {
    let input = test_data_pseudorandom(2000);
    let compressed = wado_compress2(&input, 6);
    let decompressed = zlib_rs_decompress(&compressed, input.len() * 2);
    assert_eq!(
        decompressed, input,
        "pseudo-random data cross-inflate failed"
    );
}

// ============================================================================
// Large data (> 32KB sliding window boundary)
// ============================================================================

#[test]
fn zlib_rs_large_sequential_inflated_by_wado() {
    let input: Vec<u8> = (0..34054).map(|i| (i % 256) as u8).collect();
    let compressed = zlib_rs_compress(&input, 6);
    let decompressed = wado_inflate_zlib(&compressed);
    assert_eq!(decompressed, input, "Wado inflate failed for large data");
}

#[test]
fn wado_compress_large_sequential_decompressed_by_zlib_rs() {
    let input: Vec<u8> = (0..34054).map(|i| (i % 256) as u8).collect();
    let compressed = wado_compress2(&input, 6);
    let decompressed = zlib_rs_decompress(&compressed, input.len() * 2);
    assert_eq!(
        decompressed, input,
        "zlib-rs failed to decompress Wado large data"
    );
}

#[test]
fn cross_validate_large_repetitive() {
    let input = test_data_repeated(b'X', 5000);
    let compressed = wado_compress2(&input, 6);
    let decompressed = zlib_rs_decompress(&compressed, input.len() * 2);
    assert_eq!(
        decompressed, input,
        "large repetitive data cross-inflate failed"
    );
}

// ============================================================================
// Streaming API cross-validation
// ============================================================================

#[test]
fn wado_deflate_stream_raw_decompressed_by_flate2() {
    let input = b"Raw deflate test data";
    let compressed = wado_deflate_stream_raw(input, 6);
    let decompressed = flate2_inflate_raw(&compressed);
    assert_eq!(
        decompressed, input,
        "DeflateStream RAW_FORMAT cross-inflate failed"
    );
}

#[test]
fn wado_deflate_stream_gzip_decompressed_by_flate2() {
    let input = b"Gzip format test data";
    let compressed = wado_deflate_stream_gzip(input, 6);
    let decompressed = flate2_gzip_decompress(&compressed);
    assert_eq!(
        decompressed, input,
        "DeflateStream GZIP_FORMAT cross-inflate failed"
    );
}

#[test]
fn wado_inflate_stream_raw_from_flate2() {
    let input = b"Inflate raw from flate2";
    let compressed = flate2_deflate_raw(input, 6);
    let decompressed = wado_inflate_stream_raw(&compressed);
    assert_eq!(
        decompressed, input,
        "InflateStream RAW_FORMAT from flate2 failed"
    );
}

#[test]
fn wado_inflate_stream_gzip_from_flate2() {
    let input = b"Inflate gzip from flate2";
    let compressed = flate2_gzip_compress(input, 6);
    let decompressed = wado_inflate_stream_gzip(&compressed);
    assert_eq!(
        decompressed, input,
        "InflateStream GZIP_FORMAT from flate2 failed"
    );
}

#[test]
fn wado_inflate_stream_auto_format_zlib() {
    let input = b"Auto detect zlib";
    let compressed = zlib_rs_compress(input, 6);
    let decompressed = wado_inflate_stream_auto(&compressed);
    assert_eq!(
        decompressed, input,
        "InflateStream AUTO_FORMAT with zlib input failed"
    );
}

#[test]
fn wado_inflate_stream_auto_format_gzip() {
    let input = b"Auto detect gzip";
    let compressed = flate2_gzip_compress(input, 6);
    let decompressed = wado_inflate_stream_auto(&compressed);
    assert_eq!(
        decompressed, input,
        "InflateStream AUTO_FORMAT with gzip input failed"
    );
}

// ============================================================================
// compress_bound and version
// ============================================================================

#[test]
fn compress_bound_matches_zlib_rs() {
    let sizes = [0, 1, 100, 1000, 10000, 65535];
    for &size in &sizes {
        let wado_bound = wado_compress_bound(size as i32);
        let rs_bound = zlib_rs::compress_bound(size);
        assert!(
            wado_bound as usize >= rs_bound,
            "compress_bound({size}): wado={wado_bound} < zlib-rs={rs_bound}",
        );
    }
}

#[test]
fn wado_zlib_version() {
    assert_eq!(wado_version(), "1.3.1.wado");
}

// ============================================================================
// Compression size comparison (within 10%)
// ============================================================================

fn assert_size_within_10_percent(wado_len: usize, rs_len: usize, label: &str) {
    let max_len = wado_len.max(rs_len);
    let min_len = wado_len.min(rs_len);
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
        let wado_compressed = wado_compress2(&input, level);
        let rs_compressed = zlib_rs_compress(&input, level);
        let decompressed = zlib_rs_decompress(&wado_compressed, input.len() * 2);
        assert_eq!(
            decompressed, input,
            "zlib-rs failed to decompress Wado level {level}"
        );
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
        let wado_compressed = wado_compress2(&input, level);
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
        let wado_compressed = wado_compress2(&input, level);
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

// ============================================================================
// Fuzz: random data, random levels, random formats
// ============================================================================

#[test]
fn fuzz_wado_compress_zlib_rs_decompress() {
    let mut rng: u32 = date_seed().wrapping_mul(31);
    let mut next = || -> u32 {
        rng = rng.wrapping_mul(1_103_515_245).wrapping_add(12345);
        rng >> 16
    };

    for i in 0..20 {
        let size = (next() % 3000) as usize;
        let level = (next() % 10) as i32; // 0-9
        let data: Vec<u8> = (0..size).map(|_| next() as u8).collect();

        let compressed = wado_compress2(&data, level);
        let decompressed = zlib_rs_decompress(&compressed, data.len() + 1024);
        assert_eq!(
            decompressed, data,
            "fuzz #{i}: Wado compress level {level}, size {size}"
        );
    }
}

#[test]
fn fuzz_zlib_rs_compress_wado_decompress() {
    let mut rng: u32 = date_seed().wrapping_mul(37);
    let mut next = || -> u32 {
        rng = rng.wrapping_mul(1_103_515_245).wrapping_add(12345);
        rng >> 16
    };

    for i in 0..20 {
        let size = (next() % 3000) as usize;
        let level = (next() % 10) as i32; // 0-9
        let data: Vec<u8> = (0..size).map(|_| next() as u8).collect();

        let compressed = zlib_rs_compress(&data, level);
        let decompressed = wado_inflate_zlib(&compressed);
        assert_eq!(
            decompressed, data,
            "fuzz #{i}: zlib-rs compress level {level}, size {size}"
        );
    }
}

#[test]
fn fuzz_raw_deflate_round_trip() {
    let mut rng: u32 = date_seed().wrapping_mul(41);
    let mut next = || -> u32 {
        rng = rng.wrapping_mul(1_103_515_245).wrapping_add(12345);
        rng >> 16
    };

    for i in 0..10 {
        let size = (next() % 2000) as usize;
        let level = (next() % 10) as i32;
        let data: Vec<u8> = (0..size).map(|_| next() as u8).collect();

        // Wado deflate → flate2 inflate
        let compressed = wado_deflate_raw(&data, level);
        let decompressed = flate2_inflate_raw(&compressed);
        assert_eq!(
            decompressed, data,
            "fuzz #{i}: Wado raw deflate level {level}, size {size}"
        );

        // flate2 deflate → Wado inflate
        let compressed2 = flate2_deflate_raw(&data, level as u32);
        let decompressed2 = wado_inflate_raw(&compressed2);
        assert_eq!(
            decompressed2, data,
            "fuzz #{i}: flate2 raw deflate level {level}, size {size}"
        );
    }
}

#[test]
fn fuzz_gzip_round_trip() {
    let mut rng: u32 = date_seed().wrapping_mul(43);
    let mut next = || -> u32 {
        rng = rng.wrapping_mul(1_103_515_245).wrapping_add(12345);
        rng >> 16
    };

    for i in 0..10 {
        let size = (next() % 2000) as usize;
        let level = (next() % 10) as i32;
        let data: Vec<u8> = (0..size).map(|_| next() as u8).collect();

        // Wado gzip → flate2 decompress
        let compressed = wado_gzip_compress(&data, level);
        let decompressed = flate2_gzip_decompress(&compressed);
        assert_eq!(
            decompressed, data,
            "fuzz #{i}: Wado gzip level {level}, size {size}"
        );

        // flate2 gzip → Wado decompress
        let compressed2 = flate2_gzip_compress(&data, level as u32);
        let decompressed2 = wado_inflate_gzip(&compressed2);
        assert_eq!(
            decompressed2, data,
            "fuzz #{i}: flate2 gzip level {level}, size {size}"
        );
    }
}
