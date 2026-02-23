use std::io::{self, Read, Write};

fn main() {
    // Read all gzip data from stdin
    let mut input = Vec::new();
    io::stdin().read_to_end(&mut input).unwrap();

    // Decompress gzip and write to stdout
    let mut output = vec![0u8; 8192];
    let config = zlib_rs::InflateConfig { window_bits: 31 };
    let (decompressed, rc) = zlib_rs::decompress_slice(&mut output, &input, config);
    assert!(rc == zlib_rs::ReturnCode::Ok);

    io::stdout().write_all(decompressed).unwrap();
}
