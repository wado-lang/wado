//! Wado bundled libraries
//!
//! This crate provides bundled functionality compiled to WebAssembly,
//! including the ryu float formatting library for deterministic
//! float-to-string conversion.
//!
//! This is a minimal verification implementation that demonstrates:
//! 1. ryuクレートの統合
//! 2. Wasmへのコンパイル
//! 3. 浮動小数点数の文字列化
//!
//! 現時点では、Core Wasm（cdylib）としてビルドされます。
//! 将来的には、Component Modelへの変換を追加する予定です。

/// Format an f64 to a string using ryu
///
/// # Example
/// ```
/// let result = wado_bundled::format_f64(1.23456);
/// assert_eq!(result, "1.23456");
/// ```
pub fn format_f64(value: f64) -> String {
    let mut buf = ryu::Buffer::new();
    buf.format(value).to_string()
}

/// Format an f32 to a string using ryu
///
/// # Example
/// ```
/// let result = wado_bundled::format_f32(1.234_f32);
/// assert_eq!(result, "1.234");
/// ```
pub fn format_f32(value: f32) -> String {
    let mut buf = ryu::Buffer::new();
    buf.format(value).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_f64() {
        assert_eq!(format_f64(1.23456), "1.23456");
        assert_eq!(format_f64(0.0), "0.0");
        assert_eq!(format_f64(-1.5), "-1.5");
        assert_eq!(format_f64(1e10), "10000000000.0");
    }

    #[test]
    fn test_format_f32() {
        assert_eq!(format_f32(1.234_f32), "1.234");
        assert_eq!(format_f32(0.0_f32), "0.0");
        assert_eq!(format_f32(-1.5_f32), "-1.5");
    }

    #[test]
    fn test_determinism() {
        // Same value should always produce same string
        let value = 1.2345678901234567_f64;
        for _ in 0..100 {
            assert_eq!(format_f64(value), "1.2345678901234567");
        }
    }
}
