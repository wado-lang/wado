//! Cache-key composition for Kiln generator invocations.
//!
//! The cache key is a SHA-256 over a canonical byte string described in
//! WEP 2026-04-12 §"Cache-key composition". This module is the single
//! authority for that layout: callers (the pipeline driver, the lockfile
//! writer) go through [`compose_cache_key`] so a cache hit is bit-identical
//! regardless of who computed it.
//!
//! The options encoding is intentionally opaque here (`&[u8]`). The caller
//! supplies already-canonical bytes:
//!
//! - M3 driver: provisional TOML-tree encoding produced outside the
//!   compiler; since M6.4 it also emits canonical JSON so the two paths
//!   stay byte-equivalent on the common subset.
//! - M6.4 driver: canonical JSON encoding produced by
//!   [`encode_options_canonical`]. The same bytes feed the cache key and
//!   the wire-form `options: string` on `core:kiln/types::raw-request`.
//!
//! Swapping the encoder in one place keeps cache keys stable unless the
//! user-facing options actually change.

use sha2::{Digest, Sha256};

use super::invocation::{GeneratorModule, Invocation, InvocationPath};
use super::options::CanonicalValue;
use super::options_check::CanonicalOptions;

fn digest(bytes: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(bytes);
    h.finalize().into()
}

fn hex(bytes: &[u8; 32]) -> String {
    const CHARS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(64);
    for b in bytes {
        out.push(CHARS[(b >> 4) as usize] as char);
        out.push(CHARS[(b & 0x0f) as usize] as char);
    }
    out
}

/// Magic + version prefix. Bump when the canonical layout changes in a
/// way that must invalidate every existing lockfile entry. `v2` marked
/// the M6.4 switch from the binary options encoder to canonical JSON;
/// `v3` drops NFC normalization on string values so cache keys reflect
/// the literal UTF-8 bytes the user supplied.
const MAGIC: &[u8] = b"kiln-cache-key-v3\0";

/// The core:kiln world version the generator was built against. Part of the
/// cache key so a future world-version bump invalidates every cached entry.
/// Kept in sync with `wado-compiler/lib/core/kiln/generator.wit`'s
/// `package core:kiln@...;` declaration.
const WORLD_VERSION: &[u8] = b"core:kiln@0.1.0\0";

/// Ingredients for [`compose_cache_key`].
///
/// The fields are ordered exactly as they appear in the canonical byte
/// stream; edit the comments together with the hashing loop below.
pub struct CacheKeyInputs<'a> {
    /// `"ns:name@resolved-version"` of the generator — resolved from the
    /// build-dependency lockfile entry, not the spec string.
    pub generator_identity: &'a str,
    /// Raw SHA-256 of the generator's source distribution (from the lockfile).
    pub generator_source_hash: &'a [u8; 32],
    /// The primary schema file: path + SHA-256 of contents.
    pub primary: &'a FileHash,
    /// Declared supplementary inputs, in declaration order.
    pub inputs: &'a [FileHash],
    /// Files the previous run of this invocation read via `host::read-file`,
    /// sorted lexicographically by path and deduplicated.
    pub prior_reads: &'a [FileHash],
    /// Canonical encoding of the options (from the invocation).
    pub options_canonical: &'a [u8],
}

/// A single file identity — path + content hash — used in cache keys.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileHash {
    pub path: String,
    pub hash: [u8; 32],
}

/// Compose the cache key. Returns the raw 32-byte SHA-256 digest.
#[must_use]
pub fn compose_cache_key(inputs: &CacheKeyInputs<'_>) -> [u8; 32] {
    let mut h = Sha256::new();
    write_prefixed(&mut h, MAGIC);
    write_prefixed(&mut h, WORLD_VERSION);
    write_prefixed(&mut h, inputs.generator_identity.as_bytes());
    h.update(inputs.generator_source_hash);

    write_file_hash(&mut h, inputs.primary);
    write_u32(&mut h, inputs.inputs.len() as u32);
    for f in inputs.inputs {
        write_file_hash(&mut h, f);
    }
    write_u32(&mut h, inputs.prior_reads.len() as u32);
    for f in inputs.prior_reads {
        write_file_hash(&mut h, f);
    }

    write_prefixed(&mut h, inputs.options_canonical);
    h.finalize().into()
}

/// Hex-encode a 32-byte digest.
#[must_use]
pub fn hex_digest(digest: &[u8; 32]) -> String {
    hex(digest)
}

/// Encode a [`CanonicalOptions`] to the canonical cache-key / wire byte
/// string (canonical JSON, RFC 8785–style).
///
/// - Object keys are lexicographically sorted (UTF-8 byte order).
/// - String values are written as their literal UTF-8 bytes and escaped
///   minimally: `"`, `\`, and the ASCII control set use `\"`, `\\`,
///   `\b`, `\f`, `\n`, `\r`, `\t`, or `\uXXXX`. Non-ASCII characters
///   survive as raw UTF-8 bytes; no Unicode normalization is applied,
///   so two strings that differ only in NFC vs NFD form hash to
///   different cache keys (intended: keys reflect literal input).
/// - Integers render as shortest base-10 with no leading sign on zero.
/// - Floats render shortest-roundtrip. Integer-valued floats drop their
///   trailing `.0`. `-0.0` canonicalizes to `0`. `NaN` / `±Inf` panic
///   because JSON cannot represent them — no Wado options schema yields
///   them.
/// - `Option::None` fields are omitted from the enclosing object.
///   `Some(x)` is transparent: the JSON value is exactly `x`'s encoding.
/// - Enum variants render as JSON strings using their Wado name.
#[must_use]
pub fn encode_options_canonical(options: &CanonicalOptions) -> Vec<u8> {
    let mut out = Vec::new();
    encode_options_table(&mut out, &options.values);
    out
}

fn encode_options_table(out: &mut Vec<u8>, entries: &[(String, CanonicalValue)]) {
    out.push(b'{');
    let mut indices: Vec<usize> = (0..entries.len())
        .filter(|i| !matches!(entries[*i].1, CanonicalValue::None))
        .collect();
    indices.sort_by(|a, b| entries[*a].0.cmp(&entries[*b].0));
    let mut first = true;
    for i in indices {
        if !first {
            out.push(b',');
        }
        first = false;
        let (k, v) = &entries[i];
        write_json_string(out, k);
        out.push(b':');
        encode_canonical_value(out, v);
    }
    out.push(b'}');
}

fn encode_canonical_value(out: &mut Vec<u8>, v: &CanonicalValue) {
    match v {
        CanonicalValue::Bool(true) => out.extend_from_slice(b"true"),
        CanonicalValue::Bool(false) => out.extend_from_slice(b"false"),
        CanonicalValue::I64(n) => {
            use std::io::Write;
            let _ = write!(out, "{n}");
        }
        CanonicalValue::U64(n) => {
            use std::io::Write;
            let _ = write!(out, "{n}");
        }
        CanonicalValue::F64(f) => {
            assert!(
                f.is_finite(),
                "kiln: canonical JSON cannot encode non-finite float {f}"
            );
            write_json_float(out, *f);
        }
        CanonicalValue::String(s) | CanonicalValue::Enum(s) => {
            write_json_string(out, s);
        }
        CanonicalValue::None => {
            panic!("kiln: encode_options_canonical reached bare None value");
        }
        CanonicalValue::Some(inner) => {
            encode_canonical_value(out, inner);
        }
        CanonicalValue::Struct(fields) => {
            encode_options_table(out, fields);
        }
    }
}

/// Write a JSON string literal: surrounding quotes, literal UTF-8
/// payload, minimal escapes. No Unicode normalization is applied —
/// see the encoder docstring above for rationale.
fn write_json_string(out: &mut Vec<u8>, s: &str) {
    out.push(b'"');
    for ch in s.chars() {
        match ch {
            '"' => out.extend_from_slice(b"\\\""),
            '\\' => out.extend_from_slice(b"\\\\"),
            '\u{0008}' => out.extend_from_slice(b"\\b"),
            '\u{000C}' => out.extend_from_slice(b"\\f"),
            '\n' => out.extend_from_slice(b"\\n"),
            '\r' => out.extend_from_slice(b"\\r"),
            '\t' => out.extend_from_slice(b"\\t"),
            c if (c as u32) < 0x20 => {
                use std::io::Write;
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => {
                let mut buf = [0u8; 4];
                out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            }
        }
    }
    out.push(b'"');
}

fn write_json_float(out: &mut Vec<u8>, f: f64) {
    use std::io::Write;
    if f == 0.0 {
        out.push(b'0');
        return;
    }
    if f.fract() == 0.0 && f.abs() < 9_007_199_254_740_992.0 {
        let _ = write!(out, "{}", f as i64);
        return;
    }
    let _ = write!(out, "{f}");
}

/// Hex SHA-256 of the canonical options encoding.
///
/// Written as `options-hash` in `wado.lock` so humans can diff options
/// without reverse-engineering the cache key.
#[must_use]
pub fn hash_options_canonical(options_canonical: &[u8]) -> String {
    hex(&digest(options_canonical))
}

/// Raw SHA-256 of a byte slice. Use when the caller needs a digest
/// independent of any path (e.g., comparing on-disk contents to a cached
/// hex digest).
#[must_use]
pub fn content_hash(contents: &[u8]) -> [u8; 32] {
    digest(contents)
}

impl FileHash {
    /// Build a [`FileHash`] from a pre-computed content digest. The path is
    /// normalized. Cheap — no re-hash of the contents.
    #[must_use]
    pub fn from_content(path: &InvocationPath, hash: [u8; 32]) -> Self {
        Self {
            path: path.as_str().to_string(),
            hash,
        }
    }
}

/// Build a `FileHash` from raw bytes, normalizing the path.
#[must_use]
pub fn file_hash(path: &InvocationPath, contents: &[u8]) -> FileHash {
    FileHash::from_content(path, content_hash(contents))
}

/// Collect the minimal set of hashing inputs for a single invocation.
///
/// `read_primary` and `read_input` are callbacks so the composer doesn't need
/// to touch the filesystem; they receive the normalized path as it would
/// appear in the lockfile.
///
/// # Errors
/// Propagates any error from the callbacks.
pub fn gather_file_hashes<E, P, I>(
    invocation: &Invocation,
    mut read_primary: P,
    mut read_input: I,
) -> Result<(FileHash, Vec<FileHash>), E>
where
    P: FnMut(&InvocationPath) -> Result<Vec<u8>, E>,
    I: FnMut(&InvocationPath) -> Result<Vec<u8>, E>,
{
    let primary_bytes = read_primary(&invocation.from)?;
    let primary = file_hash(&invocation.from, &primary_bytes);
    let mut inputs = Vec::with_capacity(invocation.inputs.len());
    for path in &invocation.inputs {
        let bytes = read_input(path)?;
        inputs.push(file_hash(path, &bytes));
    }
    Ok((primary, inputs))
}

/// Resolve the generator's identity string for the cache key.
///
/// For [`GeneratorModule::Spec`] the string is returned verbatim — callers are
/// expected to pass the resolved `"ns:name@<resolved-version>"` form. For
/// [`GeneratorModule::LocalPath`] a synthetic identity is derived so cached
/// entries for two different local paths do not collide.
#[must_use]
pub fn generator_identity(module: &GeneratorModule) -> String {
    match module {
        GeneratorModule::Spec(s) => s.clone(),
        GeneratorModule::LocalPath(p) => format!("local:{}", p.as_str()),
    }
}

fn write_prefixed(h: &mut Sha256, bytes: &[u8]) {
    h.update((bytes.len() as u64).to_be_bytes());
    h.update(bytes);
}

fn write_u32(h: &mut Sha256, n: u32) {
    h.update(n.to_be_bytes());
}

fn write_file_hash(h: &mut Sha256, f: &FileHash) {
    write_prefixed(h, f.path.as_bytes());
    h.update(f.hash);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kiln::invocation::{DeclSite, GeneratorModule, Invocation};

    fn fh(path: &str, hash_byte: u8) -> FileHash {
        FileHash {
            path: path.to_string(),
            hash: [hash_byte; 32],
        }
    }

    fn base_inputs<'a>(
        primary: &'a FileHash,
        zero: &'a [u8; 32],
        options: &'a [u8],
    ) -> CacheKeyInputs<'a> {
        CacheKeyInputs {
            generator_identity: "ns:proto@1.0.0",
            generator_source_hash: zero,
            primary,
            inputs: &[],
            prior_reads: &[],
            options_canonical: options,
        }
    }

    #[test]
    fn compose_is_deterministic() {
        let zero = [0u8; 32];
        let primary = fh("schema.proto", 0xaa);
        let a = compose_cache_key(&base_inputs(&primary, &zero, b""));
        let b = compose_cache_key(&base_inputs(&primary, &zero, b""));
        assert_eq!(a, b);
    }

    #[test]
    fn compose_changes_with_options() {
        let zero = [0u8; 32];
        let primary = fh("schema.proto", 0xaa);
        let a = compose_cache_key(&base_inputs(&primary, &zero, b""));
        let b = compose_cache_key(&base_inputs(&primary, &zero, b"\x01\x02\x03"));
        assert_ne!(a, b);
    }

    #[test]
    fn compose_changes_with_generator_identity() {
        let zero = [0u8; 32];
        let primary = fh("schema.proto", 0xaa);
        let mut inputs = base_inputs(&primary, &zero, b"");
        let a = compose_cache_key(&inputs);
        inputs.generator_identity = "ns:proto@2.0.0";
        let b = compose_cache_key(&inputs);
        assert_ne!(a, b);
    }

    #[test]
    fn compose_changes_with_source_hash() {
        let zero = [0u8; 32];
        let one = [1u8; 32];
        let primary = fh("schema.proto", 0xaa);
        let a = compose_cache_key(&base_inputs(&primary, &zero, b""));
        let b = compose_cache_key(&base_inputs(&primary, &one, b""));
        assert_ne!(a, b);
    }

    #[test]
    fn compose_distinguishes_input_from_prior_read() {
        let zero = [0u8; 32];
        let primary = fh("schema.proto", 0xaa);
        let extra = fh("included.proto", 0xbb);
        let a = compose_cache_key(&CacheKeyInputs {
            generator_identity: "ns:proto@1.0.0",
            generator_source_hash: &zero,
            primary: &primary,
            inputs: std::slice::from_ref(&extra),
            prior_reads: &[],
            options_canonical: b"",
        });
        let b = compose_cache_key(&CacheKeyInputs {
            generator_identity: "ns:proto@1.0.0",
            generator_source_hash: &zero,
            primary: &primary,
            inputs: &[],
            prior_reads: &[extra],
            options_canonical: b"",
        });
        assert_ne!(a, b);
    }

    #[test]
    fn generator_identity_for_spec_is_verbatim() {
        let m = GeneratorModule::Spec("ns:proto@1.0.0".to_string());
        assert_eq!(generator_identity(&m), "ns:proto@1.0.0");
    }

    #[test]
    fn generator_identity_for_local_uses_path() {
        let m = GeneratorModule::LocalPath(InvocationPath::normalize("./tools/gen"));
        assert_eq!(generator_identity(&m), "local:tools/gen");
    }

    #[test]
    fn hash_options_canonical_is_hex() {
        let h = hash_options_canonical(b"");
        assert_eq!(h.len(), 64);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn file_hash_stores_normalized_path() {
        let p = InvocationPath::normalize("./schema.proto");
        let fh = file_hash(&p, b"syntax = \"proto3\";");
        assert_eq!(fh.path, "schema.proto");
    }

    #[test]
    fn content_hash_matches_file_hash_content_field() {
        let p = InvocationPath::normalize("schema.proto");
        let bytes = b"syntax = \"proto3\";";
        let via_file_hash = file_hash(&p, bytes).hash;
        let direct = content_hash(bytes);
        assert_eq!(direct, via_file_hash);
        let via_from_content = FileHash::from_content(&p, direct);
        assert_eq!(via_from_content.path, "schema.proto");
        assert_eq!(via_from_content.hash, direct);
    }

    #[test]
    fn gather_file_hashes_threads_errors() {
        let inv = Invocation {
            decl_site: DeclSite::Manifest {
                name: "proto".to_string(),
            },
            module: GeneratorModule::Spec("ns:p@1.0.0".to_string()),
            from: InvocationPath::normalize("schema.proto"),
            inputs: vec![InvocationPath::normalize("dep.proto")],
            output_dir: InvocationPath::normalize("build/kiln/proto"),
            options_canonical: vec![],
            raw_options: None,
        };
        let result: Result<(FileHash, Vec<FileHash>), &'static str> =
            gather_file_hashes(&inv, |_| Err("oops"), |_| Ok(vec![]));
        assert_eq!(result, Err("oops"));
    }

    use crate::kiln::options::OptionsDescriptor;
    use crate::kiln::options_check::CanonicalOptions;

    fn opts(pairs: Vec<(&str, CanonicalValue)>) -> CanonicalOptions {
        CanonicalOptions {
            descriptor: OptionsDescriptor { fields: vec![] },
            values: pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
        }
    }

    #[test]
    fn canonical_json_empty_table_is_braces() {
        let bytes = encode_options_canonical(&opts(vec![]));
        assert_eq!(bytes, b"{}");
    }

    #[test]
    fn canonical_json_bool_int_u64_render_plainly() {
        let bytes = encode_options_canonical(&opts(vec![
            ("flag", CanonicalValue::Bool(true)),
            ("n", CanonicalValue::I64(-7)),
            ("u", CanonicalValue::U64(42)),
        ]));
        assert_eq!(
            std::str::from_utf8(&bytes).unwrap(),
            r#"{"flag":true,"n":-7,"u":42}"#
        );
    }

    #[test]
    fn canonical_json_integer_float_drops_point_zero() {
        let bytes = encode_options_canonical(&opts(vec![("ratio", CanonicalValue::F64(5.0))]));
        assert_eq!(std::str::from_utf8(&bytes).unwrap(), r#"{"ratio":5}"#);
    }

    #[test]
    fn canonical_json_string_escapes_quote_and_controls() {
        let bytes = encode_options_canonical(&opts(vec![(
            "msg",
            CanonicalValue::String("a\"b\nc\td".to_string()),
        )]));
        assert_eq!(
            std::str::from_utf8(&bytes).unwrap(),
            r#"{"msg":"a\"b\nc\td"}"#
        );
    }

    #[test]
    fn canonical_json_string_preserves_literal_utf8_without_normalization() {
        let decomposed = "e\u{0301}";
        let composed = "\u{00e9}";
        let bytes_d = encode_options_canonical(&opts(vec![(
            "accent",
            CanonicalValue::String(decomposed.to_string()),
        )]));
        let bytes_c = encode_options_canonical(&opts(vec![(
            "accent",
            CanonicalValue::String(composed.to_string()),
        )]));
        assert_ne!(
            bytes_d, bytes_c,
            "literal UTF-8 bytes must survive the encoder; NFC vs NFD differ"
        );
        assert!(std::str::from_utf8(&bytes_d).unwrap().contains(decomposed));
        assert!(std::str::from_utf8(&bytes_c).unwrap().contains(composed));
    }

    #[test]
    fn canonical_json_sorts_keys_regardless_of_input_order() {
        let unsorted = encode_options_canonical(&opts(vec![
            ("zeta", CanonicalValue::I64(1)),
            ("alpha", CanonicalValue::I64(2)),
            ("mu", CanonicalValue::I64(3)),
        ]));
        let sorted = encode_options_canonical(&opts(vec![
            ("alpha", CanonicalValue::I64(2)),
            ("mu", CanonicalValue::I64(3)),
            ("zeta", CanonicalValue::I64(1)),
        ]));
        assert_eq!(unsorted, sorted);
        assert_eq!(
            std::str::from_utf8(&unsorted).unwrap(),
            r#"{"alpha":2,"mu":3,"zeta":1}"#
        );
    }

    #[test]
    fn canonical_json_option_none_is_omitted_and_some_is_transparent() {
        let bytes = encode_options_canonical(&opts(vec![
            ("kept", CanonicalValue::Bool(true)),
            ("dropped", CanonicalValue::None),
            (
                "wrapped",
                CanonicalValue::Some(Box::new(CanonicalValue::String("x".to_string()))),
            ),
        ]));
        assert_eq!(
            std::str::from_utf8(&bytes).unwrap(),
            r#"{"kept":true,"wrapped":"x"}"#
        );
    }

    #[test]
    fn canonical_json_float_negative_zero_renders_as_zero() {
        let bytes = encode_options_canonical(&opts(vec![("z", CanonicalValue::F64(-0.0))]));
        assert_eq!(std::str::from_utf8(&bytes).unwrap(), r#"{"z":0}"#);
    }

    #[test]
    fn canonical_json_struct_nests_cleanly() {
        let bytes = encode_options_canonical(&opts(vec![(
            "nested",
            CanonicalValue::Struct(vec![
                ("y".to_string(), CanonicalValue::I64(2)),
                ("x".to_string(), CanonicalValue::I64(1)),
            ]),
        )]));
        assert_eq!(
            std::str::from_utf8(&bytes).unwrap(),
            r#"{"nested":{"x":1,"y":2}}"#
        );
    }
}
