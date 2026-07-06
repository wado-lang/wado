//! Shared dependency-cache layout: the ghq-style path a fetched registry
//! artifact occupies under the cache root (`~/wado/`, see the CLI-subcommands
//! WEP). Kept here — pure, portable string logic with no filesystem or
//! environment access — so both the CLI (which resolves the root and fetches)
//! and the language server (which reads a warm cache offline) derive an
//! identical path from one place.

/// The cache-root-relative path (forward-slash separated) a registry artifact
/// occupies: `{host}/{prefix?}/{ns}/{name}/[{world_segment}/]{version}/component.wasm`.
///
/// `registry_url` is an `oci://<host>[/<prefix>]` URL; `coordinate` is an
/// `ns:pkg` package. `world_segment` names a non-library world sub-path (a Kiln
/// generator publishes to `core-kiln-generator`); `None` is the package's
/// library component. Returns `None` for a non-`oci://` URL or a bare
/// coordinate — mirroring `wado publish`'s push layout, so a fetched artifact
/// caches where it was pulled from.
#[must_use]
pub fn registry_cache_relative(
    registry_url: &str,
    coordinate: &str,
    world_segment: Option<&str>,
    version: &str,
) -> Option<String> {
    let stripped = registry_url.strip_prefix("oci://")?.trim_matches('/');
    let (host, prefix) = match stripped.split_once('/') {
        Some((h, p)) => (h, p.trim_matches('/')),
        None => (stripped, ""),
    };
    let (namespace, name) = coordinate.split_once(':')?;

    let mut parts = vec![host];
    if !prefix.is_empty() {
        parts.push(prefix);
    }
    parts.push(namespace);
    parts.push(name);
    if let Some(segment) = world_segment {
        parts.push(segment);
    }
    parts.push(version);
    parts.push("component.wasm");
    Some(parts.join("/"))
}

#[cfg(test)]
mod tests {
    use super::registry_cache_relative;

    #[test]
    fn library_component_layout() {
        assert_eq!(
            registry_cache_relative("oci://ghcr.io", "wado-lang:cm-catalog", None, "0.1.0"),
            Some("ghcr.io/wado-lang/cm-catalog/0.1.0/component.wasm".to_string())
        );
    }

    #[test]
    fn registry_prefix_is_kept() {
        assert_eq!(
            registry_cache_relative("oci://ghcr.io/acme", "ns:pkg", None, "1.2.3"),
            Some("ghcr.io/acme/ns/pkg/1.2.3/component.wasm".to_string())
        );
    }

    #[test]
    fn generator_world_subpath() {
        assert_eq!(
            registry_cache_relative(
                "oci://ghcr.io",
                "wado-lang:gale",
                Some("core-kiln-generator"),
                "0.0.9"
            ),
            Some("ghcr.io/wado-lang/gale/core-kiln-generator/0.0.9/component.wasm".to_string())
        );
    }

    #[test]
    fn rejects_non_oci_url_and_bare_coordinate() {
        assert!(registry_cache_relative("https://wa.dev", "ns:pkg", None, "1.0.0").is_none());
        assert!(registry_cache_relative("oci://ghcr.io", "pkg", None, "1.0.0").is_none());
    }
}
