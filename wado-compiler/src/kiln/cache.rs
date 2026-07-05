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
//! a deterministic, injective serialization of the validated options tree used
//! solely as a hash input. In protocol v0.3 the options cross the generator
//! boundary as a typed WIT argument, not a serialized blob, so nothing ever
//! decodes these bytes — the encoding only has to be canonical, not an
//! interchange format.
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
/// the literal UTF-8 bytes the user supplied; `v4` switched the options
/// encoding to CBOR; `v5` (protocol revision v0.3) replaces CBOR with a
/// tagged length-prefixed hash-only encoding when options became a typed
/// WIT argument.
const MAGIC: &[u8] = b"kiln-cache-key-v5\0";

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

/// Encode a [`CanonicalOptions`] to a deterministic byte string for the
/// invocation cache key. This is purely a hash input — nothing ever decodes
/// it (options now cross the generator boundary as a typed WIT argument, not a
/// serialized blob) — so the format only has to be canonical and injective,
/// not an interchange format.
///
/// A table drops `None` options, sorts the rest by field name, and writes a
/// length-prefixed entry list; each value is a one-byte type tag followed by
/// its little-endian / length-prefixed bytes (`Some(x)` is transparent). `F64`
/// canonicalizes `-0.0` to `0.0` and rejects non-finite values so equal option
/// sets share a cache key.
#[must_use]
pub fn encode_options_canonical(options: &CanonicalOptions) -> Vec<u8> {
    let mut out = Vec::new();
    encode_options_table(&mut out, &options.values);
    out
}

/// The "no options" encoding (an empty table). The cache-key fallback whenever
/// a generator has no options descriptor.
#[must_use]
pub fn empty_options_canonical() -> Vec<u8> {
    let mut out = Vec::new();
    encode_options_table(&mut out, &[]);
    out
}

fn write_len(out: &mut Vec<u8>, len: usize) {
    out.extend_from_slice(&(len as u64).to_le_bytes());
}

fn write_str(out: &mut Vec<u8>, s: &str) {
    write_len(out, s.len());
    out.extend_from_slice(s.as_bytes());
}

fn encode_options_table(out: &mut Vec<u8>, entries: &[(String, CanonicalValue)]) {
    let mut kept: Vec<&(String, CanonicalValue)> = entries
        .iter()
        .filter(|(_, v)| !matches!(v, CanonicalValue::None))
        .collect();
    kept.sort_by(|a, b| a.0.cmp(&b.0));

    write_len(out, kept.len());
    for (k, v) in kept {
        write_str(out, k);
        encode_canonical_value(out, v);
    }
}

fn encode_canonical_value(out: &mut Vec<u8>, v: &CanonicalValue) {
    match v {
        CanonicalValue::Bool(b) => {
            out.push(0);
            out.push(u8::from(*b));
        }
        CanonicalValue::I64(n) => {
            out.push(1);
            out.extend_from_slice(&n.to_le_bytes());
        }
        CanonicalValue::U64(n) => {
            out.push(2);
            out.extend_from_slice(&n.to_le_bytes());
        }
        CanonicalValue::F64(f) => {
            // Non-finite floats are rejected during options validation
            // (`options_check`), so they never reach the encoder. `-0.0`
            // canonicalizes to `0.0` so `+0.0`/`-0.0` share a cache key.
            assert!(
                f.is_finite(),
                "kiln: options cannot encode non-finite float {f}"
            );
            let f = if *f == 0.0 { 0.0 } else { *f };
            out.push(3);
            out.extend_from_slice(&f.to_le_bytes());
        }
        CanonicalValue::String(s) => {
            out.push(4);
            write_str(out, s);
        }
        CanonicalValue::Enum(s) => {
            out.push(5);
            write_str(out, s);
        }
        CanonicalValue::None => {
            panic!("kiln: encode_options_canonical reached bare None value");
        }
        CanonicalValue::Some(inner) => encode_canonical_value(out, inner),
        CanonicalValue::Struct(fields) => {
            out.push(6);
            encode_options_table(out, fields);
        }
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
            options: crate::kiln::options_check::CanonicalOptions::default(),
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

    // The cache-key encoding is a hash input, not an interchange format, so
    // these assert the properties the cache key depends on directly on the
    // bytes: determinism, distinctness, `None`-drop, `Some`-transparency, and
    // `-0.0`/`+0.0` collapsing.

    #[test]
    fn empty_table_equals_empty_options_canonical() {
        assert_eq!(
            empty_options_canonical(),
            encode_options_canonical(&opts(vec![]))
        );
    }

    #[test]
    fn distinct_option_sets_encode_distinctly() {
        let a = encode_options_canonical(&opts(vec![
            ("flag", CanonicalValue::Bool(true)),
            ("n", CanonicalValue::I64(-7)),
            ("u", CanonicalValue::U64(42)),
        ]));
        assert_ne!(
            a,
            encode_options_canonical(&opts(vec![
                ("flag", CanonicalValue::Bool(false)),
                ("n", CanonicalValue::I64(-7)),
                ("u", CanonicalValue::U64(42)),
            ]))
        );
        assert_ne!(
            a,
            encode_options_canonical(&opts(vec![("ratio", CanonicalValue::F64(5.5))]))
        );
    }

    #[test]
    fn negative_zero_canonicalizes_to_positive_zero() {
        let pos = encode_options_canonical(&opts(vec![("z", CanonicalValue::F64(0.0))]));
        let neg = encode_options_canonical(&opts(vec![("z", CanonicalValue::F64(-0.0))]));
        assert_eq!(pos, neg);
    }

    #[test]
    fn string_and_enum_of_the_same_text_encode_distinctly() {
        let s = encode_options_canonical(&opts(vec![(
            "x",
            CanonicalValue::String("Rpc".to_string()),
        )]));
        let e = encode_options_canonical(&opts(vec![("x", CanonicalValue::Enum("Rpc".to_string()))]));
        assert_ne!(s, e, "a string and an enum are distinct value kinds");
    }

    #[test]
    fn string_preserves_literal_utf8_without_normalization() {
        let decomposed = "e\u{0301}";
        let composed = "\u{00e9}";
        let bytes_d = encode_options_canonical(&opts(vec![(
            "a",
            CanonicalValue::String(decomposed.into()),
        )]));
        let bytes_c =
            encode_options_canonical(&opts(vec![("a", CanonicalValue::String(composed.into()))]));
        assert_ne!(bytes_d, bytes_c, "NFC vs NFD must survive the encoder");
    }

    #[test]
    fn output_is_deterministic_regardless_of_input_order() {
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
        assert_eq!(a, b, "reordered fields must encode identically");
    }

    #[test]
    fn option_none_is_omitted_and_some_is_transparent() {
        // A `None` field encodes the same as omitting it, and `Some(x)` the
        // same as bare `x`.
        let with_none = encode_options_canonical(&opts(vec![
            ("kept", CanonicalValue::Bool(true)),
            ("dropped", CanonicalValue::None),
            (
                "wrapped",
                CanonicalValue::Some(Box::new(CanonicalValue::String("x".to_string()))),
            ),
        ]));
        let without = encode_options_canonical(&opts(vec![
            ("kept", CanonicalValue::Bool(true)),
            ("wrapped", CanonicalValue::String("x".to_string())),
        ]));
        assert_eq!(with_none, without);
    }

    #[test]
    fn struct_field_order_is_normalized() {
        let a = encode_options_canonical(&opts(vec![(
            "nested",
            CanonicalValue::Struct(vec![
                ("y".to_string(), CanonicalValue::I64(2)),
                ("x".to_string(), CanonicalValue::I64(1)),
            ]),
        )]));
        let b = encode_options_canonical(&opts(vec![(
            "nested",
            CanonicalValue::Struct(vec![
                ("x".to_string(), CanonicalValue::I64(1)),
                ("y".to_string(), CanonicalValue::I64(2)),
            ]),
        )]));
        assert_eq!(a, b, "a nested struct's field order must not affect the key");
    }
}
