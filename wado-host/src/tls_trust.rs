//! Shared rustls trust-anchor configuration for the CLI's outbound TLS:
//! `wado run`'s high-level `wasi:http` client (`wado-cli`'s `http_hooks`),
//! its raw `wasi:tls` connector (its `runtime`), and the OCI registry client
//! (its `oci`).
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

use std::path::Path;
use std::sync::Once;

use rustls::RootCertStore;
use rustls::pki_types::CertificateDer;
use rustls::pki_types::pem::PemObject;

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

/// Build a `RootCertStore` containing `webpki-roots` plus [`extra_ca_certs`].
pub fn build_root_cert_store() -> RootCertStore {
    let mut roots = RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.into(),
    };
    for cert in extra_ca_certs() {
        roots
            .add(cert)
            .expect("extra_ca_certs keeps only trust anchors");
    }
    roots
}

/// The CA certificates `WADO_CA_BUNDLE` / `SSL_CERT_FILE` / `SSL_CERT_DIR`
/// name, each one usable as a trust anchor. An unreadable file or an entry
/// that is not a CA certificate is reported and skipped, so that one bad
/// entry does not silently disable trust for the rest.
pub fn extra_ca_certs() -> Vec<CertificateDer<'static>> {
    let mut certs = Vec::new();
    for var in ["WADO_CA_BUNDLE", "SSL_CERT_FILE"] {
        if let Ok(path) = std::env::var(var)
            && !path.is_empty()
        {
            certs.extend(ca_certs_in_file(Path::new(&path)));
        }
    }
    if let Ok(dir) = std::env::var("SSL_CERT_DIR")
        && !dir.is_empty()
    {
        certs.extend(ca_certs_in_dir(Path::new(&dir)));
    }
    certs
}

fn ca_certs_in_dir(dir: &Path) -> Vec<CertificateDer<'static>> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        warn_log!("failed to read SSL_CERT_DIR {}", dir.display());
        return Vec::new();
    };
    entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_file())
        .flat_map(|path| ca_certs_in_file(&path))
        .collect()
}

fn ca_certs_in_file(path: &Path) -> Vec<CertificateDer<'static>> {
    match std::fs::read(path) {
        Ok(pem) => ca_certs_in_pem(&pem, path),
        Err(err) => {
            warn_log!("failed to read CA bundle {}: {err}", path.display());
            Vec::new()
        }
    }
}

// The warnings name the file and nothing read out of it: CodeQL's
// `rust/cleartext-logging` treats whatever comes from a certificate as
// sensitive, a count of them included.
fn ca_certs_in_pem(pem: &[u8], origin: &Path) -> Vec<CertificateDer<'static>> {
    let mut certs = Vec::new();
    for item in CertificateDer::pem_slice_iter(pem) {
        let Ok(cert) = item else {
            warn_log!("skipped malformed PEM in {}", origin.display());
            continue;
        };
        if webpki::anchor_from_trusted_cert(&cert).is_ok() {
            certs.push(cert);
        } else {
            warn_log!(
                "skipped a certificate in {} that is not a valid CA certificate",
                origin.display()
            );
        }
    }
    certs
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROOT_CA: &str = include_str!("../testdata/isrg_root_x2.pem");
    const NOT_A_CERTIFICATE: &str =
        "-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n";

    #[test]
    fn skips_an_entry_that_is_not_a_ca_and_keeps_the_rest() {
        let pem = format!("{NOT_A_CERTIFICATE}{ROOT_CA}");
        let certs = ca_certs_in_pem(pem.as_bytes(), Path::new("ca.pem"));
        assert_eq!(certs.len(), 1);
    }
}
