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

use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Once};

use indexmap::IndexSet;
use rustls::RootCertStore;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, TrustAnchor};

macro_rules! warn_log {
    ($($arg:tt)*) => { eprintln!("warning: {}", format_args!($($arg)*)) };
}

/// A certificate the environment adds to the trust store, with the anchor
/// rustls reads it as.
type ExtraCa = (CertificateDer<'static>, TrustAnchor<'static>);

/// Read once per process: every client shares them, and a bad entry warns once.
static EXTRA_CAS: LazyLock<Vec<ExtraCa>> = LazyLock::new(|| extra_cas_in(bundle_paths()));

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
    roots
        .roots
        .extend(EXTRA_CAS.iter().map(|(_, anchor)| anchor.clone()));
    roots
}

/// The certificates `WADO_CA_BUNDLE` / `SSL_CERT_FILE` / `SSL_CERT_DIR` name,
/// each once. An unreadable file or an entry that does not parse as a trust
/// anchor is reported and skipped, so that one bad entry does not silently
/// disable trust for the rest. Like OpenSSL's, the bundle is trusted as given:
/// a self-signed server certificate in it is an anchor too.
pub fn extra_ca_certs() -> impl Iterator<Item = &'static CertificateDer<'static>> {
    EXTRA_CAS.iter().map(|(der, _)| der)
}

fn bundle_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for var in ["WADO_CA_BUNDLE", "SSL_CERT_FILE"] {
        if let Ok(path) = std::env::var(var)
            && !path.is_empty()
        {
            paths.push(PathBuf::from(path));
        }
    }
    if let Ok(dir) = std::env::var("SSL_CERT_DIR")
        && !dir.is_empty()
    {
        match std::fs::read_dir(&dir) {
            Ok(entries) => paths.extend(
                entries
                    .flatten()
                    .map(|entry| entry.path())
                    .filter(|path| path.is_file()),
            ),
            Err(err) => warn_log!("failed to read SSL_CERT_DIR {dir}: {err}"),
        }
    }
    paths
}

/// The anchors in `paths`, each file and each certificate taken once: an
/// OpenSSL-style `SSL_CERT_DIR` holds every CA as a file, a hash-named
/// symlink to it, and a bundle of them all.
fn extra_cas_in(paths: Vec<PathBuf>) -> Vec<ExtraCa> {
    let mut files = IndexSet::new();
    let mut certs = IndexSet::new();
    let mut cas = Vec::new();
    for path in paths {
        let read = std::fs::canonicalize(&path).and_then(|real| {
            if !files.insert(real) {
                return Ok(None);
            }
            std::fs::read(&path).map(Some)
        });
        let pem = match read {
            Ok(Some(pem)) => pem,
            Ok(None) => continue,
            Err(err) => {
                warn_log!("failed to read CA bundle {}: {err}", path.display());
                continue;
            }
        };
        for ca in extra_cas_in_pem(&pem, &path) {
            if certs.insert(ca.0.clone()) {
                cas.push(ca);
            }
        }
    }
    cas
}

// The warnings name the file and nothing read out of it: CodeQL's
// `rust/cleartext-logging` treats whatever comes from a certificate as
// sensitive, a count of them included.
fn extra_cas_in_pem(pem: &[u8], origin: &Path) -> Vec<ExtraCa> {
    let mut cas = Vec::new();
    for item in CertificateDer::pem_slice_iter(pem) {
        let Ok(der) = item else {
            warn_log!("skipped malformed PEM in {}", origin.display());
            continue;
        };
        match webpki::anchor_from_trusted_cert(&der) {
            Ok(anchor) => {
                let anchor = anchor.to_owned();
                cas.push((der, anchor));
            }
            Err(_) => warn_log!(
                "skipped a certificate in {} that does not parse as a trust anchor",
                origin.display()
            ),
        }
    }
    cas
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROOT_CA: &str = include_str!("../testdata/isrg_root_x2.pem");
    const UNPARSEABLE: &str = "-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n";
    const UNTERMINATED: &str = "-----BEGIN CERTIFICATE-----\nAAAA\n";

    #[test]
    fn skips_an_entry_that_does_not_parse_and_keeps_the_rest() {
        let pem = format!("{UNPARSEABLE}{ROOT_CA}");
        let cas = extra_cas_in_pem(pem.as_bytes(), Path::new("ca.pem"));
        assert_eq!(cas.len(), 1);
    }

    #[test]
    fn skips_malformed_pem() {
        let pem = format!("{ROOT_CA}{UNTERMINATED}");
        let cas = extra_cas_in_pem(pem.as_bytes(), Path::new("ca.pem"));
        assert_eq!(cas.len(), 1);
    }

    #[test]
    fn takes_each_certificate_once_across_symlinks_and_bundles() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("isrg.pem");
        let link = dir.path().join("4042bcee.0");
        let bundle = dir.path().join("bundle.crt");
        std::fs::write(&file, ROOT_CA).unwrap();
        std::os::unix::fs::symlink(&file, &link).unwrap();
        std::fs::write(&bundle, format!("{ROOT_CA}{ROOT_CA}")).unwrap();
        assert_eq!(extra_cas_in(vec![file, link, bundle]).len(), 1);
    }
}
