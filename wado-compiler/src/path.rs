//! Deterministic, host-independent lexical path normalization.
//!
//! A Wado module path is a filesystem representation, not a URI: a space stays
//! a space and a literal `%` stays a `%`. Normalization here is purely lexical
//! — it folds `.`/`..` and redundant separators without touching the
//! filesystem (so it is `wasm32-unknown-unknown`-safe) and without consulting
//! the host platform's path parser (so a module identity is byte-identical on
//! every host). It follows the RFC 3986 §5.2.4 dot-segment semantics, extended
//! to preserve an absolute root (POSIX `/` or a Windows drive prefix), which a
//! `..` segment can never escape.

/// Normalize a filesystem-style path lexically.
///
/// - `\` is treated as `/` (Windows separators are unified).
/// - `.` segments are dropped; `..` pops the preceding real segment.
/// - An absolute root (`/` or `C:`) is preserved and `..` cannot escape it.
/// - A relative path keeps its leading `./` marker and any leading `..`.
/// - The content is never percent-encoded, decoded, or otherwise rewritten,
///   so the round-trip back to a filesystem path is the identity function.
///
/// Examples:
/// - `./sub/../geometry.wado` → `./geometry.wado`
/// - `/abs/a/../b.wado` → `/abs/b.wado`
/// - `/home/user/My Project/x.wado` → `/home/user/My Project/x.wado`
/// - `C:\proj\main.wado` → `C:/proj/main.wado`
#[must_use]
pub fn normalize(path: &str) -> String {
    let unified = path.replace('\\', "/");

    let (root, rest) = split_root(&unified);
    let is_absolute = !root.is_empty();
    let explicit_dot = !is_absolute && rest.starts_with("./");

    let mut segments: Vec<&str> = Vec::new();
    for segment in rest.split('/') {
        match segment {
            "" | "." => {}
            ".." => match segments.last() {
                Some(&last) if last != ".." => {
                    segments.pop();
                }
                // `..` at the absolute root is a no-op; relative paths keep it.
                _ if is_absolute => {}
                _ => segments.push(".."),
            },
            s => segments.push(s),
        }
    }

    let body = segments.join("/");

    if is_absolute {
        if body.is_empty() {
            // `/` → `/`, `C:` → `C:/`
            if root.ends_with('/') {
                root
            } else {
                format!("{root}/")
            }
        } else if root.ends_with('/') {
            format!("{root}{body}")
        } else {
            format!("{root}/{body}")
        }
    } else if body.is_empty() {
        ".".to_string()
    } else if explicit_dot && !body.starts_with("..") {
        format!("./{body}")
    } else {
        body
    }
}

/// Split an absolute root off the front of `path`, returning `(root, rest)`.
///
/// `root` is `"/"` for a POSIX-absolute path or `"C:"` for a Windows drive
/// prefix, and is empty for a relative path. `rest` is the remainder with any
/// leading separator removed, ready to be split into segments.
fn split_root(path: &str) -> (String, &str) {
    if let Some(stripped) = path.strip_prefix('/') {
        return ("/".to_string(), stripped);
    }
    let bytes = path.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        let root = path[..2].to_string();
        let rest = path[2..].strip_prefix('/').unwrap_or(&path[2..]);
        return (root, rest);
    }
    (String::new(), path)
}

/// Express `target` as a path relative to the directory `base`, lexically.
///
/// Both arguments are normalized first ([`normalize`]). The result is the
/// minimal `./`- or `../`-prefixed path that, joined with `base` and
/// normalized, yields `target` again — the unique spelling of `target` as seen
/// from `base`. Purely lexical (no filesystem, `wasm32`-safe).
///
/// `base` and `target` must share rootedness (both relative, or both absolute
/// under the same root); a mismatch returns `normalize(target)` unchanged.
///
/// Examples:
/// - base `/p/src`, target `/p/src/gen/x.wado` → `./gen/x.wado`
/// - base `/p/src`, target `/p/shared/h.wado` → `../shared/h.wado`
/// - base `.`, target `./x.wado` → `./x.wado`
#[must_use]
pub fn relative_path(base: &str, target: &str) -> String {
    let base = normalize(base);
    let target = normalize(target);
    let (base_root, base_rest) = split_root(&base);
    let (target_root, target_rest) = split_root(&target);
    if base_root != target_root {
        return target;
    }

    // `normalize` drops every `..` from an absolute path, so only relative
    // paths carry them; a `..` at the same index is the same ancestor on both
    // sides, so string equality gives the common prefix. Drop `.` (a `.` base or
    // a leading `./`) — it would add a phantom segment that inflates the climb
    // count (e.g. `relative_path(".", "../x")` → `../../x`).
    let drop = |s: &&str| !s.is_empty() && *s != ".";
    let base_comps: Vec<&str> = base_rest.split('/').filter(drop).collect();
    let target_comps: Vec<&str> = target_rest.split('/').filter(drop).collect();

    let common = base_comps
        .iter()
        .zip(&target_comps)
        .take_while(|(a, b)| a == b)
        .count();

    // Climb out of every base component past the common prefix (a trailing
    // `..` is itself climbed with a `..`), then descend into target's tail.
    let ups = base_comps.len() - common;
    let mut out: Vec<&str> = std::iter::repeat_n("..", ups).collect();
    out.extend(&target_comps[common..]);

    if out.is_empty() {
        ".".to_string()
    } else if out[0] == ".." {
        out.join("/")
    } else {
        format!("./{}", out.join("/"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_path_under_base() {
        assert_eq!(relative_path("/p/src", "/p/src/gen/x.wado"), "./gen/x.wado");
        assert_eq!(relative_path("/p/src", "/p/src/x.wado"), "./x.wado");
        assert_eq!(relative_path(".", "./x.wado"), "./x.wado");
        assert_eq!(relative_path("src", "src/gen/x.wado"), "./gen/x.wado");
    }

    #[test]
    fn relative_path_above_base() {
        assert_eq!(
            relative_path("/p/src", "/p/shared/h.wado"),
            "../shared/h.wado"
        );
        assert_eq!(relative_path("/p/a/b", "/p/x.wado"), "../../x.wado");
        assert_eq!(relative_path("src", "shared/h.wado"), "../shared/h.wado");
    }

    #[test]
    fn relative_path_escape_reentry_canonicalizes() {
        // The #1423 case: `..`-escape that re-enters the base must collapse to
        // the direct spelling — base joined with the non-canonical form
        // normalizes to the same target, and `relative_path` yields the minimal
        // form.
        let base = "/p/src";
        let target = normalize(&format!("{base}/../src/gen/parser.wado"));
        assert_eq!(target, "/p/src/gen/parser.wado");
        assert_eq!(relative_path(base, &target), "./gen/parser.wado");
    }

    #[test]
    fn relative_path_same_dir_and_root() {
        assert_eq!(relative_path("/p/src", "/p/src"), ".");
        assert_eq!(relative_path(".", "."), ".");
    }

    // A `.`-rooted base (the common `wado compile ./main.wado` → entry_dir `.`)
    // must not inflate the climb count with a phantom `.` segment.
    #[test]
    fn relative_path_dot_base() {
        assert_eq!(relative_path(".", "gen/x.wado"), "./gen/x.wado");
        assert_eq!(relative_path(".", "./gen/x.wado"), "./gen/x.wado");
        assert_eq!(relative_path(".", "../shared.wado"), "../shared.wado");
        assert_eq!(relative_path(".", "../../shared.wado"), "../../shared.wado");
    }

    #[test]
    fn relative_path_relative_bases_with_parent() {
        // Both climb above the cwd: the shared `..` prefix is common.
        assert_eq!(relative_path("../a", "../a/x.wado"), "./x.wado");
        assert_eq!(relative_path("../a", "../b.wado"), "../b.wado");
        assert_eq!(relative_path("..", "../../b.wado"), "../b.wado");
    }

    #[test]
    fn relative_path_root_mismatch_falls_back() {
        // No relative spelling across an absolute/relative boundary.
        assert_eq!(relative_path("/p/src", "rel/x.wado"), "rel/x.wado");
    }

    #[test]
    fn relative_dot_segments() {
        assert_eq!(normalize("./a/b/../c.wado"), "./a/c.wado");
        assert_eq!(normalize("./a/./b/c.wado"), "./a/b/c.wado");
        assert_eq!(normalize("a//b/c.wado"), "a/b/c.wado");
        assert_eq!(normalize("./sub/../geometry.wado"), "./geometry.wado");
        assert_eq!(normalize("./sub/./file.wado"), "./sub/file.wado");
        assert_eq!(normalize("./a/b/../c/./d.wado"), "./a/c/d.wado");
        assert_eq!(normalize("foo.wado"), "foo.wado");
    }

    #[test]
    fn relative_leading_parent_preserved() {
        assert_eq!(normalize("../../x.wado"), "../../x.wado");
        // An explicit `./` is dropped once the result escapes upward.
        assert_eq!(normalize("./../foo"), "../foo");
    }

    #[test]
    fn empty_and_dot_only() {
        assert_eq!(normalize(""), ".");
        assert_eq!(normalize("."), ".");
        assert_eq!(normalize("./"), ".");
    }

    #[test]
    fn absolute_root_preserved() {
        // Regression: the old relative-only normalizer dropped the leading `/`.
        assert_eq!(normalize("/abs/a/../b.wado"), "/abs/b.wado");
        assert_eq!(normalize("/a//b"), "/a/b");
        assert_eq!(normalize("/a/b/../c/./d.wado"), "/a/c/d.wado");
    }

    #[test]
    fn absolute_parent_cannot_escape_root() {
        assert_eq!(normalize("/a/../../b"), "/b");
        assert_eq!(normalize("/.."), "/");
        assert_eq!(normalize("/"), "/");
    }

    #[test]
    fn uri_unsafe_chars_are_filesystem_literal() {
        // The bug: a space must stay a space, never percent-encoded or rejected.
        assert_eq!(normalize("./a b.wado"), "./a b.wado");
        assert_eq!(
            normalize("/home/user/My Project/eval.wado"),
            "/home/user/My Project/eval.wado"
        );
        // A literal `%` is part of the filename, not an escape — leave it.
        assert_eq!(normalize("./a%20b.wado"), "./a%20b.wado");
        assert_eq!(
            normalize("/My Project/a/../b c.wado"),
            "/My Project/b c.wado"
        );
    }

    #[test]
    fn windows_separators_and_drive() {
        assert_eq!(
            normalize("C:\\My Project\\main.wado"),
            "C:/My Project/main.wado"
        );
        assert_eq!(normalize(".\\a\\b"), "./a/b");
        assert_eq!(normalize("C:\\a\\..\\b.wado"), "C:/b.wado");
        assert_eq!(normalize("C:\\"), "C:/");
    }
}
