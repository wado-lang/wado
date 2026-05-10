//! Shared rustls trust-anchor configuration for `wado run`'s outbound
//! TLS, covering both the high-level `wasi:http` client (`http_hooks`)
//! and the raw `wasi:tls` connector (`runtime`).
//!
//! `webpki-roots` (Mozilla's curated list) is the baseline. On top of
//! that we honour the same env-var conventions OpenSSL/curl use so a
//! sandbox or corporate environment that signs outgoing HTTPS with a
//! private CA can opt into trusting it without rebuilding the binary:
//!
//! - `WADO_CA_BUNDLE` — path to a single PEM bundle (Wado-specific).
//! - `SSL_CERT_FILE` — path to a single PEM bundle (OpenSSL convention).
//! - `SSL_CERT_DIR`  — directory of PEM files (OpenSSL convention).
//!
//! All three are additive: configured CAs are merged into the embedded
//! `webpki-roots` set, never replacing it.

use std::fs::File;
use std::io::BufReader;
use std::path::Path;
use std::sync::Once;

use rustls::RootCertStore;
use rustls::pki_types::CertificateDer;

macro_rules! warn_log {
    ($($arg:tt)*) => { eprintln!("warning: {}", format_args!($($arg)*)) };
}

/// Install the rustls process-level `CryptoProvider` exactly once.
///
/// The workspace pulls in multiple rustls feature combinations through
/// wasmtime's dependency graph, so the auto-detect path used by
/// `rustls::ClientConfig::builder()` and `WasiTlsCtxBuilder::new()` panics
/// with "could not automatically determine the process-level
/// `CryptoProvider`". Both `WadoHttpHooks` and the `wasi:tls` provider
/// call this before constructing a `ClientConfig`.
pub fn install_default_crypto_provider() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        // Ignore the result: another caller may have raced us, in which
        // case a provider is already in place and that is fine.
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// Build a `RootCertStore` containing `webpki-roots` plus any CA
/// certificates pointed to by `WADO_CA_BUNDLE` / `SSL_CERT_FILE` /
/// `SSL_CERT_DIR`. Per-cert / per-file errors are logged and skipped so
/// that a malformed entry in one bundle does not silently disable trust
/// for unrelated bundles.
pub fn build_root_cert_store() -> RootCertStore {
    let mut roots = RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.into(),
    };
    load_extra_ca_certs(&mut roots);
    roots
}

fn load_extra_ca_certs(roots: &mut RootCertStore) {
    for var in ["WADO_CA_BUNDLE", "SSL_CERT_FILE"] {
        if let Ok(path) = std::env::var(var)
            && !path.is_empty()
        {
            load_pem_bundle(roots, Path::new(&path));
        }
    }
    if let Ok(dir) = std::env::var("SSL_CERT_DIR")
        && !dir.is_empty()
    {
        load_pem_dir(roots, Path::new(&dir));
    }
}

fn load_pem_bundle(roots: &mut RootCertStore, path: &Path) {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(err) => {
            warn_log!("failed to open CA bundle {}: {err}", path.display());
            return;
        }
    };
    let mut reader = BufReader::new(file);
    let mut certs: Vec<CertificateDer<'static>> = Vec::new();
    for item in rustls_pemfile::certs(&mut reader) {
        match item {
            Ok(cert) => certs.push(cert),
            Err(err) => warn_log!("failed to parse cert in {}: {err}", path.display()),
        }
    }
    let (_added, ignored) = roots.add_parsable_certificates(certs);
    if ignored > 0 {
        warn_log!("ignored {ignored} invalid certs in {}", path.display());
    }
}

fn load_pem_dir(roots: &mut RootCertStore, dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        warn_log!("failed to read SSL_CERT_DIR {}", dir.display());
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file() {
            load_pem_bundle(roots, &path);
        }
    }
}
