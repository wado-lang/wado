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

    // Split off an absolute root that `..` cannot escape: POSIX `/` or a
    // Windows drive prefix like `C:` / `C:/`.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_dot_segments() {
        // Pins the historical `remove_dot_segments` behavior.
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
