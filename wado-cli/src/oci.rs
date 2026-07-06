//! Minimal OCI registry client for pulling Wado package dependencies.
//!
//! Consuming a registry dependency must not require an external `wkg` binary
//! (publishing shells out to `wkg`, but every build would otherwise need it),
//! so pulls go through a native client ([`oci_client`], the same crate `wkg`
//! uses). A published Wado package is a standalone Wasm Component Model
//! artifact — a `application/wasm` layer under a `application/vnd.wasm.config`
//! config — so resolution needs only the manifest digest (integrity) and, when
//! building, the component bytes.
//!
//! Authentication mirrors `wado publish`: the `WKG_OCI_USERNAME` /
//! `WKG_OCI_PASSWORD` environment variables, else an anonymous pull.

use oci_client::client::{Certificate, CertificateEncoding, ClientConfig};
use oci_client::secrets::RegistryAuth;
use oci_client::{Client, Reference};

/// The OCI layer media type `wado publish` (via `wkg`) writes for a component.
const WASM_LAYER_MEDIA_TYPE: &str = "application/wasm";

/// Build the OCI [`Reference`] for a package coordinate at a version.
///
/// The registry URL is `oci://<host>[/<prefix>]` (from `[registries]`); an open
/// coordinate `ns:pkg` maps to the repository `<host>/<prefix>/<ns>/<pkg>` with
/// the version as the image tag, matching `wado publish`'s push layout.
pub fn reference(registry_url: &str, package: &str, tag: &str) -> Result<Reference, String> {
    let stripped = registry_url
        .strip_prefix("oci://")
        .ok_or_else(|| format!("registry {registry_url:?} is not an oci:// URL"))?
        .trim_matches('/');
    let (host, prefix) = match stripped.split_once('/') {
        Some((h, p)) => (h, p.trim_matches('/')),
        None => (stripped, ""),
    };
    let (namespace, name) = package
        .split_once(':')
        .ok_or_else(|| format!("package {package:?} is not an `ns:pkg` coordinate"))?;
    let mut repository = String::new();
    if !prefix.is_empty() {
        repository.push_str(prefix);
        repository.push('/');
    }
    repository.push_str(namespace);
    repository.push('/');
    repository.push_str(name);
    Ok(Reference::with_tag(
        host.to_string(),
        repository,
        tag.to_string(),
    ))
}

/// List the image tags for a package's repository.
pub async fn list_tags(reference: &Reference) -> anyhow::Result<Vec<String>> {
    let resp = client().list_tags(reference, &auth(), None, None).await?;
    Ok(resp.tags)
}

/// The content digest (`sha256:…`) of the artifact manifest at `reference`,
/// used as the lock file's `integrity`. Fetches only the manifest, not the
/// component blob.
pub async fn manifest_digest(reference: &Reference) -> anyhow::Result<String> {
    let (_manifest, digest) = client().pull_manifest(reference, &auth()).await?;
    Ok(digest)
}

/// Pull the component's Wasm bytes (the single `application/wasm` layer).
pub async fn pull_component(reference: &Reference) -> anyhow::Result<Vec<u8>> {
    let data = client()
        .pull(reference, &auth(), vec![WASM_LAYER_MEDIA_TYPE])
        .await?;
    let layer = data
        .layers
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("no {WASM_LAYER_MEDIA_TYPE} layer in {reference}"))?;
    Ok(layer.data)
}

fn client() -> Client {
    let config = ClientConfig {
        extra_root_certificates: extra_root_certificates(),
        ..ClientConfig::default()
    };
    Client::new(config)
}

fn auth() -> RegistryAuth {
    match (
        std::env::var("WKG_OCI_USERNAME"),
        std::env::var("WKG_OCI_PASSWORD"),
    ) {
        (Ok(user), Ok(password)) => RegistryAuth::Basic(user, password),
        _ => RegistryAuth::Anonymous,
    }
}

/// Extra trust roots from `SSL_CERT_FILE`, added to the default webpki roots so
/// a custom or corporate CA (e.g. an intercepting proxy) is trusted without
/// weakening verification for public registries. Absent or unreadable file
/// yields no extra roots.
fn extra_root_certificates() -> Vec<Certificate> {
    let Some(path) = std::env::var_os("SSL_CERT_FILE") else {
        return Vec::new();
    };
    let Ok(bytes) = std::fs::read(&path) else {
        return Vec::new();
    };
    let mut reader = std::io::BufReader::new(bytes.as_slice());
    rustls_pemfile::certs(&mut reader)
        .filter_map(Result::ok)
        .map(|der| Certificate {
            encoding: CertificateEncoding::Der,
            data: der.to_vec(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reference_maps_coordinate_to_repository() {
        let r = reference("oci://ghcr.io", "wado-lang:cm-catalog", "0.0.9").unwrap();
        assert_eq!(r.registry(), "ghcr.io");
        assert_eq!(r.repository(), "wado-lang/cm-catalog");
        assert_eq!(r.tag(), Some("0.0.9"));
    }

    #[test]
    fn reference_includes_registry_prefix() {
        let r = reference("oci://ghcr.io/acme", "ns:pkg", "1.2.3").unwrap();
        assert_eq!(r.registry(), "ghcr.io");
        assert_eq!(r.repository(), "acme/ns/pkg");
    }

    #[test]
    fn reference_rejects_non_oci_url() {
        assert!(reference("https://wa.dev", "ns:pkg", "1.0.0").is_err());
    }

    #[test]
    fn reference_rejects_bare_coordinate() {
        assert!(reference("oci://ghcr.io", "pkg", "1.0.0").is_err());
    }
}
