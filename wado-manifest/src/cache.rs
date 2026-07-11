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

/// The cache-root-relative path of a git dependency's canonical clone:
/// `{host}/{owner}/{repo}`. This is the ghq-compatible checkout that hosts the
/// shared object store; per-version worktrees nest under it (see
/// [`git_worktree_relative`]).
///
/// `url` is any git URL — `https://host/owner/repo(.git)`,
/// `git@host:owner/repo(.git)`, or `ssh://git@host/owner/repo` — normalized to
/// `host/owner/repo`. Returns `None` when the URL has no `host/owner/repo` shape.
#[must_use]
pub fn git_repo_relative(url: &str) -> Option<String> {
    parse_git_url(url)
}

/// The cache-root-relative path of a git dependency's per-version worktree:
/// `{host}/{owner}/{repo}/.worktrees/{version}-{short-ref}`. `short-ref` is the
/// first 8 hex of `resolved_ref` (or the whole ref when shorter). Nesting under
/// the canonical clone keeps `ghq list` to one entry per repo. Returns `None`
/// for an unparseable URL.
#[must_use]
pub fn git_worktree_relative(url: &str, version: &str, resolved_ref: &str) -> Option<String> {
    let repo = git_repo_relative(url)?;
    let short = &resolved_ref[..resolved_ref.len().min(8)];
    Some(format!("{repo}/.worktrees/{version}-{short}"))
}

/// Normalize a git URL to `{host}/{owner}/{repo}`, stripping a scheme, an
/// optional `user@`, a trailing `.git`, and any trailing slash. Handles the
/// scp-like `git@host:owner/repo` form (colon after the host) as well as URL
/// forms. Returns `None` when the URL lacks a `host/owner/repo` shape.
fn parse_git_url(url: &str) -> Option<String> {
    // Strip a scheme (`https://`, `ssh://`, `git://`, …).
    let after_scheme = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    // Strip an optional `user@`.
    let after_user = after_scheme
        .rsplit_once('@')
        .map(|(_, rest)| rest)
        .unwrap_or(after_scheme);
    // The scp-like form uses `host:owner/repo`; URL forms use `host/owner/repo`.
    // A `:` before the first `/` is the scp separator, normalized to `/`.
    let normalized = match (after_user.find(':'), after_user.find('/')) {
        (Some(colon), slash) if slash.is_none_or(|s| colon < s) => {
            format!("{}/{}", &after_user[..colon], &after_user[colon + 1..])
        }
        _ => after_user.to_string(),
    };
    let trimmed = normalized.trim_end_matches('/');
    let trimmed = trimmed.strip_suffix(".git").unwrap_or(trimmed);
    let segments: Vec<&str> = trimmed.split('/').filter(|s| !s.is_empty()).collect();
    // A `file://` URL or a bare absolute path (used for local repos and tests)
    // has no `host/owner/repo` prefix, so key it on the trailing three path
    // components. A remote URL keeps its full path so nested groups (GitLab
    // subgroups `host/group/sub/repo`) stay distinct; `host/owner/repo` is the
    // common three-segment case.
    let is_local = url.starts_with("file://") || after_scheme.starts_with('/');
    if is_local {
        match segments.as_slice() {
            [.., host, owner, repo] => Some(format!("{host}/{owner}/{repo}")),
            _ => None,
        }
    } else if segments.len() >= 3 {
        Some(segments.join("/"))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::{git_repo_relative, git_worktree_relative, registry_cache_relative};

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

    #[test]
    fn git_repo_layout_across_url_forms() {
        for url in [
            "https://github.com/user/router.git",
            "https://github.com/user/router",
            "git@github.com:user/router.git",
            "ssh://git@github.com/user/router.git",
            "git://github.com/user/router.git/",
        ] {
            assert_eq!(
                git_repo_relative(url).as_deref(),
                Some("github.com/user/router"),
                "url {url:?}"
            );
        }
    }

    #[test]
    fn git_worktree_nests_under_the_repo_with_short_ref() {
        assert_eq!(
            git_worktree_relative(
                "https://github.com/user/router.git",
                "1.0.2",
                "abc1234def5678901234567890abcdef12345678"
            ),
            Some("github.com/user/router/.worktrees/1.0.2-abc1234d".to_string())
        );
    }

    #[test]
    fn git_short_ref_tolerates_a_short_sha() {
        assert_eq!(
            git_worktree_relative("https://x/o/r.git", "0.1.0", "abc"),
            Some("x/o/r/.worktrees/0.1.0-abc".to_string())
        );
    }

    #[test]
    fn git_rejects_remote_urls_without_owner_repo() {
        assert!(git_repo_relative("https://github.com/only-owner").is_none());
        assert!(git_repo_relative("not a url").is_none());
    }

    #[test]
    fn git_remote_url_keeps_nested_subgroup_path() {
        assert_eq!(
            git_repo_relative("https://gitlab.com/group/sub/repo.git").as_deref(),
            Some("gitlab.com/group/sub/repo")
        );
        assert_eq!(
            git_worktree_relative(
                "https://gitlab.com/group/sub/repo.git",
                "1.0.0",
                "abcd1234ef"
            )
            .as_deref(),
            Some("gitlab.com/group/sub/repo/.worktrees/1.0.0-abcd1234")
        );
    }

    #[test]
    fn git_local_and_file_urls_key_on_trailing_segments() {
        assert_eq!(
            git_repo_relative("file:///tmp/t/github.com/user/router").as_deref(),
            Some("github.com/user/router")
        );
        assert_eq!(
            git_repo_relative("/home/me/src/acme/widget").as_deref(),
            Some("src/acme/widget")
        );
    }
}
