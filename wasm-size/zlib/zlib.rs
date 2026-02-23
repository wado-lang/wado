use std::io::{self, Read};

fn main() {
    // Read all gzip data from stdin
    let mut input = Vec::new();
    io::stdin().read_to_end(&mut input).unwrap();

    // Decompress gzip (window_bits = 16 + 15 = 31 enables gzip format)
    let mut output = vec![0u8; 4096];
    let config = zlib_rs::InflateConfig { window_bits: 31 };
    let (decompressed, rc) = zlib_rs::decompress_slice(&mut output, &input, config);
    assert!(rc == zlib_rs::ReturnCode::Ok);

    println!("zlib-rs: {} -> {}", input.len(), decompressed.len());
}
