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
//! A pull authenticates with what `docker login` stored for the registry's
//! host, else pulls anonymously.

use std::fs::File;
use std::io::{BufReader, ErrorKind, Read};
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex};

use bytes::Bytes;
use docker_credential::{CredentialRetrievalError, DockerCredential};
use oci_client::client::{Certificate, CertificateEncoding, ClientConfig};
use oci_client::secrets::RegistryAuth;
use oci_client::{Client, Reference};
use wado_compiler::hashmap::IndexMap;
use wado_host::sync::{joined, lock};
use wado_host::tls_trust::extra_ca_certs;
use wado_lsp::host::discovery::home_dir;

pub use oci_client::Reference as OciReference;

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

/// Build the OCI [`Reference`] for a package's *world sub-path* — the layout
/// `wado publish` uses for a non-library world (`ns:pkg` at world `<segment>`
/// pushes to `<host>/<prefix>/<ns>/<pkg>/<segment>:<tag>`). A Kiln generator
/// lives at the `core-kiln-generator` segment of its package.
pub fn world_reference(
    registry_url: &str,
    package: &str,
    world_segment: &str,
    tag: &str,
) -> Result<Reference, String> {
    let base = reference(registry_url, package, tag)?;
    let repository = format!("{}/{world_segment}", base.repository());
    Ok(Reference::with_tag(
        base.registry().to_string(),
        repository,
        tag.to_string(),
    ))
}

/// List the image tags for a package's repository.
pub async fn list_tags(reference: &Reference) -> anyhow::Result<Vec<String>> {
    let auth = auth(reference).await?;
    let resp = client().list_tags(reference, &auth, None, None).await?;
    Ok(resp.tags)
}

/// The content digest (`sha256:…`) of the artifact manifest at `reference`,
/// used as the lock file's `integrity`. Fetches only the manifest, not the
/// component blob.
pub async fn manifest_digest(reference: &Reference) -> anyhow::Result<String> {
    let auth = auth(reference).await?;
    let (_manifest, digest) = client().pull_manifest(reference, &auth).await?;
    Ok(digest)
}

/// Pull the component's Wasm bytes (the single `application/wasm` layer).
pub async fn pull_component(reference: &Reference) -> anyhow::Result<Bytes> {
    let auth = auth(reference).await?;
    let data = client()
        .pull(reference, &auth, vec![WASM_LAYER_MEDIA_TYPE])
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

/// What a credential helper prints for a host it holds nothing for. The Docker
/// CLI matches the same text (`credentials.IsErrCredentialsNotFoundMessage`).
const HELPER_HOLDS_NONE: &str = "credentials not found in native keychain";

/// The key Docker Hub's credential is filed under, whichever of its names a
/// reference spells: `docker login` keeps the legacy index URL.
const DOCKER_HUB_KEY: &str = "https://index.docker.io/v1/";

/// What [`auth`] answered for each host this run. One resolution makes
/// several calls to a host, and a lookup can start a credential helper that
/// prompts the user.
static AUTH_BY_HOST: LazyLock<Mutex<IndexMap<String, RegistryAuth>>> =
    LazyLock::new(Mutex::default);

/// The credential `docker login` stored for `reference`'s host, else none.
///
/// It is looked up by that host and sent to no other: the host comes from a
/// manifest, and opening someone else's package in an editor resolves it.
async fn auth(reference: &Reference) -> anyhow::Result<RegistryAuth> {
    let host = reference.registry().to_string();
    if let Some(known) = lock(&AUTH_BY_HOST).get(&host) {
        return Ok(known.clone());
    }
    let found = joined(
        tokio::task::spawn_blocking({
            let host = host.clone();
            move || look_up_auth(&host)
        })
        .await,
    )?;
    lock(&AUTH_BY_HOST).insert(host, found.clone());
    Ok(found)
}

fn look_up_auth(host: &str) -> anyhow::Result<RegistryAuth> {
    let Some(config) = docker_config() else {
        return Ok(RegistryAuth::Anonymous);
    };
    let file = match File::open(&config) {
        Ok(file) => file,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(RegistryAuth::Anonymous),
        Err(e) => anyhow::bail!("reading {}: {e}", config.display()),
    };
    registry_auth(BufReader::new(file), host)
        .map_err(|e| anyhow::anyhow!("the credential for {host} in {}: {e}", config.display()))
}

fn registry_auth(config: impl Read, host: &str) -> anyhow::Result<RegistryAuth> {
    let key = match host {
        "docker.io" | "index.docker.io" | "registry-1.docker.io" => DOCKER_HUB_KEY,
        _ => host,
    };
    match docker_credential::get_credential_from_reader(config, key) {
        Ok(DockerCredential::UsernamePassword(user, password)) => {
            Ok(RegistryAuth::Basic(user, password))
        }
        Ok(DockerCredential::IdentityToken(_)) => {
            anyhow::bail!("an identity token is not supported; log in with a username")
        }
        Err(CredentialRetrievalError::NoCredentialConfigured) => Ok(RegistryAuth::Anonymous),
        Err(CredentialRetrievalError::HelperFailure { stdout, .. })
            if stdout.trim() == HELPER_HOLDS_NONE =>
        {
            Ok(RegistryAuth::Anonymous)
        }
        Err(e) => Err(e.into()),
    }
}

/// Where `docker login` writes, as the Docker CLI finds it. An empty
/// `DOCKER_CONFIG` is unset: read as a path, it would name the working
/// directory, which may be a package someone else wrote.
fn docker_config() -> Option<PathBuf> {
    let dir = std::env::var_os("DOCKER_CONFIG")
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
        .or_else(|| home_dir().map(|home| home.join(".docker")))?;
    Some(dir.join("config.json"))
}

/// The CAs `wado run` trusts beyond the default webpki roots, so a registry
/// behind a custom or corporate CA (e.g. an intercepting proxy) is reached the
/// same way a guest's HTTPS is.
fn extra_root_certificates() -> Vec<Certificate> {
    extra_ca_certs()
        .map(|der| Certificate {
            encoding: CertificateEncoding::Der,
            data: der.to_vec(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `docker login ghcr.io -u user -p pw`, as `config.json` records it.
    const GHCR_LOGIN: &str = r#"{"auths": {"ghcr.io": {"auth": "dXNlcjpwdw=="}}}"#;

    #[test]
    fn a_credential_goes_only_to_its_own_host() {
        let ghcr = registry_auth(GHCR_LOGIN.as_bytes(), "ghcr.io").unwrap();
        assert!(matches!(ghcr, RegistryAuth::Basic(user, pw) if user == "user" && pw == "pw"));
        let other = registry_auth(GHCR_LOGIN.as_bytes(), "registry.example").unwrap();
        assert!(matches!(other, RegistryAuth::Anonymous));
    }

    /// A helper names Docker Hub by its legacy index URL, as `docker login`
    /// files it. The helper here is not installed, so asking it is an error,
    /// where a lookup under the wrong key would answer anonymous.
    #[test]
    fn docker_hub_is_looked_up_by_its_index_url() {
        let config = r#"{"credHelpers": {"https://index.docker.io/v1/": "wado-absent"}}"#;
        assert!(registry_auth(config.as_bytes(), "docker.io").is_err());
    }

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
