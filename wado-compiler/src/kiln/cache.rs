//! Cache-key composition for Kiln generator invocations.
//!
//! The cache key is a SHA-256 over a canonical byte string described in
//! WEP 2026-04-12 §"Cache-key composition". This module is the single
//! authority for that layout: callers (the pipeline driver, the lockfile
//! writer) go through [`compose_cache_key`] so a cache hit is bit-identical
//! regardless of who computed it.
//!
//! The options encoding is intentionally opaque here (`&[u8]`). The caller
//! supplies already-canonical bytes produced by [`encode_options_canonical`]:
//! a deterministic CBOR encoding (RFC 8949) of the validated options tree.
//! The same bytes feed the cache key and the wire-form `options: list<u8>`
//! on `core:kiln/types::raw-request`; the generator decodes them with
//! `core:cbor::from_bytes`.
//!
//! CBOR replaced the earlier canonical-JSON layer (WEP 2026-04-12, protocol
//! revision v0.2): a binary blob decodes faster and represents `i64` / `u64`
//! / `f64` / byte payloads without JSON's numeric ambiguity. Because nothing
//! re-encodes the blob on the generator side, the encoder only has to be
//! deterministic and decodable by `core:cbor` — it need not be byte-identical
//! to `core:cbor`'s own canonical serializer.
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
/// `v3` dropped NFC normalization on string values so cache keys reflect
/// the literal UTF-8 bytes the user supplied; `v4` switches the options
/// encoding from canonical JSON to CBOR (protocol revision v0.2).
const MAGIC: &[u8] = b"kiln-cache-key-v4\0";

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

/// CBOR encoding constants used by the options encoder (RFC 8949 §3).
mod cbor {
    // Major types (top 3 bits of the head byte).
    pub const UINT: u8 = 0;
    pub const NINT: u8 = 1;
    pub const TEXT: u8 = 3;
    pub const MAP: u8 = 5;
    // Additional-info values selecting the argument width.
    pub const AI_U8: u8 = 0x18;
    pub const AI_U16: u8 = 0x19;
    pub const AI_U32: u8 = 0x1a;
    pub const AI_U64: u8 = 0x1b;
    // Simple / float head bytes.
    pub const FALSE: u8 = 0xf4;
    pub const TRUE: u8 = 0xf5;
    pub const F64: u8 = 0xfb;
}

/// Encode a [`CanonicalOptions`] to the canonical cache-key / wire byte
/// string: a deterministic CBOR (RFC 8949) encoding of the options tree.
///
/// - A struct/table is a definite-length CBOR map whose entries are sorted
///   by the bytewise order of their encoded keys (RFC 8949 §4.2.1). Field
///   names are text-string keys, matching how `core:cbor` decodes a struct.
/// - Integers use the preferred (shortest) head; `I64` renders as an
///   unsigned or negative integer by sign, `U64` always unsigned.
/// - `F64` is always the 8-byte double form. `core:cbor::deserialize_f32`
///   narrows a double, so a single width decodes into both `f32` and `f64`
///   fields. `NaN` / `±Inf` panic — no Wado options schema yields them.
/// - String and enum values are text strings (enums by their Wado name).
/// - `Option::None` fields are omitted from the enclosing map. `Some(x)`
///   is transparent: the encoding is exactly `x`'s.
///
/// The result is deterministic, which is all the cache key needs; it does
/// not have to match `core:cbor`'s own canonical serializer byte-for-byte,
/// since nothing re-encodes the blob on the generator side.
#[must_use]
pub fn encode_options_canonical(options: &CanonicalOptions) -> Vec<u8> {
    let mut out = Vec::new();
    encode_options_table(&mut out, &options.values);
    out
}

fn encode_options_table(out: &mut Vec<u8>, entries: &[(String, CanonicalValue)]) {
    // Encode each surviving (key, value) into its own buffer so the map can
    // be emitted in canonical (encoded-key-bytewise) order.
    let mut pairs: Vec<(Vec<u8>, Vec<u8>)> = Vec::with_capacity(entries.len());
    for (k, v) in entries {
        if matches!(v, CanonicalValue::None) {
            continue;
        }
        let mut key = Vec::new();
        write_cbor_text(&mut key, k);
        let mut value = Vec::new();
        encode_canonical_value(&mut value, v);
        pairs.push((key, value));
    }
    pairs.sort_by(|a, b| a.0.cmp(&b.0));

    write_cbor_head(out, cbor::MAP, pairs.len() as u64);
    for (key, value) in pairs {
        out.extend_from_slice(&key);
        out.extend_from_slice(&value);
    }
}

fn encode_canonical_value(out: &mut Vec<u8>, v: &CanonicalValue) {
    match v {
        CanonicalValue::Bool(true) => out.push(cbor::TRUE),
        CanonicalValue::Bool(false) => out.push(cbor::FALSE),
        CanonicalValue::I64(n) => {
            if *n >= 0 {
                write_cbor_head(out, cbor::UINT, *n as u64);
            } else {
                write_cbor_head(out, cbor::NINT, (-1 - *n) as u64);
            }
        }
        CanonicalValue::U64(n) => write_cbor_head(out, cbor::UINT, *n),
        CanonicalValue::F64(f) => {
            assert!(
                f.is_finite(),
                "kiln: CBOR options cannot encode non-finite float {f}"
            );
            out.push(cbor::F64);
            out.extend_from_slice(&f.to_bits().to_be_bytes());
        }
        CanonicalValue::String(s) | CanonicalValue::Enum(s) => write_cbor_text(out, s),
        CanonicalValue::None => {
            panic!("kiln: encode_options_canonical reached bare None value");
        }
        CanonicalValue::Some(inner) => encode_canonical_value(out, inner),
        CanonicalValue::Struct(fields) => encode_options_table(out, fields),
    }
}

/// Write a CBOR text string: a text-major head carrying the UTF-8 byte
/// length, followed by the literal UTF-8 payload. No Unicode normalization
/// is applied, so two strings differing only in NFC vs NFD form encode
/// differently (intended: keys reflect literal input).
fn write_cbor_text(out: &mut Vec<u8>, s: &str) {
    write_cbor_head(out, cbor::TEXT, s.len() as u64);
    out.extend_from_slice(s.as_bytes());
}

/// Write a CBOR type head (major type + argument) using the preferred
/// (shortest) encoding (RFC 8949 §3, §4.2.1).
fn write_cbor_head(out: &mut Vec<u8>, major: u8, arg: u64) {
    let high = major << 5;
    if arg < 24 {
        out.push(high | arg as u8);
    } else if arg < 0x100 {
        out.push(high | cbor::AI_U8);
        out.push(arg as u8);
    } else if arg < 0x1_0000 {
        out.push(high | cbor::AI_U16);
        out.extend_from_slice(&(arg as u16).to_be_bytes());
    } else if arg < 0x1_0000_0000 {
        out.push(high | cbor::AI_U32);
        out.extend_from_slice(&(arg as u32).to_be_bytes());
    } else {
        out.push(high | cbor::AI_U64);
        out.extend_from_slice(&arg.to_be_bytes());
    }
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
        GeneratorModule::BuildDep(s) => format!("builddep:{s}"),
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
            decl_site: DeclSite {
                module: "src/main.wado".to_string(),
                synthetic_id: "kiln-proto".to_string(),
            },
            module: GeneratorModule::Spec("ns:p@1.0.0".to_string()),
            from: InvocationPath::normalize("schema.proto"),
            source: InvocationPath::normalize("./schema.proto"),
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

    use serde_cbor::Value as Cbor;

    /// Decode the encoder's output with an independent CBOR reader
    /// (`serde_cbor`) and return the top-level map. Verifying against a real
    /// decoder — rather than hand-written byte literals — checks that the
    /// output is well-formed CBOR that `core:cbor::from_bytes` can read.
    fn decode_map(bytes: &[u8]) -> std::collections::BTreeMap<Cbor, Cbor> {
        match serde_cbor::from_slice::<Cbor>(bytes).expect("encoder emits valid CBOR") {
            Cbor::Map(m) => m,
            other => panic!("expected a CBOR map, got {other:?}"),
        }
    }

    fn key(s: &str) -> Cbor {
        Cbor::Text(s.to_string())
    }

    #[test]
    fn cbor_empty_table_is_empty_map() {
        assert!(decode_map(&encode_options_canonical(&opts(vec![]))).is_empty());
    }

    #[test]
    fn cbor_bool_int_u64_decode_to_their_values() {
        let bytes = encode_options_canonical(&opts(vec![
            ("flag", CanonicalValue::Bool(true)),
            ("n", CanonicalValue::I64(-7)),
            ("u", CanonicalValue::U64(42)),
        ]));
        let m = decode_map(&bytes);
        assert_eq!(m[&key("flag")], Cbor::Bool(true));
        assert_eq!(m[&key("n")], Cbor::Integer(-7));
        assert_eq!(m[&key("u")], Cbor::Integer(42));
    }

    #[test]
    fn cbor_f64_decodes_as_float() {
        let bytes = encode_options_canonical(&opts(vec![("ratio", CanonicalValue::F64(5.5))]));
        assert_eq!(decode_map(&bytes)[&key("ratio")], Cbor::Float(5.5));
    }

    #[test]
    fn cbor_string_and_enum_decode_as_text() {
        let bytes = encode_options_canonical(&opts(vec![
            ("msg", CanonicalValue::String("a\"b".to_string())),
            ("style", CanonicalValue::Enum("Rpc".to_string())),
        ]));
        let m = decode_map(&bytes);
        assert_eq!(m[&key("msg")], Cbor::Text("a\"b".to_string()));
        assert_eq!(m[&key("style")], Cbor::Text("Rpc".to_string()));
    }

    #[test]
    fn cbor_string_preserves_literal_utf8_without_normalization() {
        let decomposed = "e\u{0301}";
        let composed = "\u{00e9}";
        let bytes_d = encode_options_canonical(&opts(vec![(
            "a",
            CanonicalValue::String(decomposed.into()),
        )]));
        let bytes_c =
            encode_options_canonical(&opts(vec![("a", CanonicalValue::String(composed.into()))]));
        assert_ne!(bytes_d, bytes_c, "NFC vs NFD must survive the encoder");
        assert_eq!(
            decode_map(&bytes_d)[&key("a")],
            Cbor::Text(decomposed.into())
        );
        assert_eq!(decode_map(&bytes_c)[&key("a")], Cbor::Text(composed.into()));
    }

    #[test]
    fn cbor_output_is_deterministic_regardless_of_input_order() {
        let a = encode_options_canonical(&opts(vec![
            ("zeta", CanonicalValue::I64(1)),
            ("alpha", CanonicalValue::I64(2)),
            ("mu", CanonicalValue::I64(3)),
        ]));
        let b = encode_options_canonical(&opts(vec![
            ("alpha", CanonicalValue::I64(2)),
            ("mu", CanonicalValue::I64(3)),
            ("zeta", CanonicalValue::I64(1)),
        ]));
        assert_eq!(a, b, "reordered fields must hash-encode identically");
        let m = decode_map(&a);
        assert_eq!(m.len(), 3);
        assert_eq!(m[&key("alpha")], Cbor::Integer(2));
    }

    #[test]
    fn cbor_option_none_is_omitted_and_some_is_transparent() {
        let bytes = encode_options_canonical(&opts(vec![
            ("kept", CanonicalValue::Bool(true)),
            ("dropped", CanonicalValue::None),
            (
                "wrapped",
                CanonicalValue::Some(Box::new(CanonicalValue::String("x".to_string()))),
            ),
        ]));
        let m = decode_map(&bytes);
        assert!(!m.contains_key(&key("dropped")), "None field is omitted");
        assert_eq!(m[&key("wrapped")], Cbor::Text("x".to_string()));
        assert_eq!(m.len(), 2);
    }

    #[test]
    fn cbor_struct_nests_as_a_sub_map() {
        let bytes = encode_options_canonical(&opts(vec![(
            "nested",
            CanonicalValue::Struct(vec![
                ("y".to_string(), CanonicalValue::I64(2)),
                ("x".to_string(), CanonicalValue::I64(1)),
            ]),
        )]));
        let Cbor::Map(inner) = &decode_map(&bytes)[&key("nested")] else {
            panic!("nested value must be a map");
        };
        assert_eq!(inner[&key("x")], Cbor::Integer(1));
        assert_eq!(inner[&key("y")], Cbor::Integer(2));
    }
}
