fn main() {
    // Test data: 1KB of patterned bytes
    let src: Vec<u8> = (0..1024).map(|i| (i % 256) as u8).collect();

    // Compress
    let bound = zlib_rs::compress_bound(src.len());
    let mut compressed = vec![0u8; bound];
    let (compressed, rc) = zlib_rs::compress_slice(&mut compressed, &src, zlib_rs::DeflateConfig::new(6));
    assert!(rc == zlib_rs::ReturnCode::Ok);
    let compressed_len = compressed.len();

    // Decompress
    let mut decompressed = vec![0u8; src.len()];
    let (decompressed, rc) =
        zlib_rs::decompress_slice(&mut decompressed, compressed, zlib_rs::InflateConfig::default());
    assert!(rc == zlib_rs::ReturnCode::Ok);
    let decompressed_len = decompressed.len();

    // Verify
    assert!(decompressed == &src[..]);

    println!(
        "zlib-rs: {} -> {} -> {}",
        src.len(),
        compressed_len,
        decompressed_len
    );
}
