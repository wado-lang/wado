//! Remote files: a `use` or a Kiln input naming an `http://` or `https://` URL,
//! and the digest that pins what the URL serves.

use std::collections::VecDeque;
use std::fmt::{self, Write};

use sha2::{Digest, Sha256};

use crate::ast::AttrValue;
use crate::compiler_host::SourceError;

const ALGORITHM: &str = "sha256:";

/// Whether `path` names a remote file rather than a local one.
#[must_use]
pub fn is_remote(path: &str) -> bool {
    path.starts_with("https://") || path.starts_with("http://")
}

/// Why `url` names no file Wado fetches, or `None` where it names one. Plain
/// `http://` leaves the bytes open to anything on the way, so it reaches only
/// this machine, as a test server does.
#[must_use]
pub fn url_fault(url: &str) -> Option<&'static str> {
    let rest = url.strip_prefix("http://")?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host = match authority.rsplit_once(':') {
        Some((host, port)) if !host.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => host,
        _ => authority,
    };
    (!matches!(host, "localhost" | "127.0.0.1" | "[::1]"))
        .then_some("`http://` names only `localhost`, `127.0.0.1` or `[::1]`; use `https://`")
}

/// The URL `relative` names beside `base`, as RFC 3986 §5 resolves a
/// reference. A URL's path is the server's, so its empty segments stay, and its
/// query and fragment are not part of the directory.
#[must_use]
pub fn resolve_url(base: &str, relative: &str) -> String {
    let authority = base.find("://").expect("a remote URL names its scheme") + 3;
    let path_at = base[authority..]
        .find(['/', '?', '#'])
        .map_or(base.len(), |at| authority + at);
    let (origin, rest) = base.split_at(path_at);
    let base_path = &rest[..rest.find(['?', '#']).unwrap_or(rest.len())];
    let (path, tail) = relative.split_at(relative.find(['?', '#']).unwrap_or(relative.len()));
    let merged = if path.starts_with('/') {
        path.to_string()
    } else {
        let dir = base_path.rfind('/').map_or("/", |at| &base_path[..=at]);
        format!("{dir}{path}")
    };
    format!("{origin}{}{tail}", remove_dot_segments(&merged))
}

/// RFC 3986 §5.2.4 over an absolute `path`: each `.` goes, and each `..` takes
/// the segment before it, an empty one included.
fn remove_dot_segments(path: &str) -> String {
    let mut kept: Vec<&str> = Vec::new();
    let mut segments = path
        .strip_prefix('/')
        .expect("a merged URL path is absolute")
        .split('/')
        .peekable();
    while let Some(segment) = segments.next() {
        match segment {
            "." => {}
            ".." => {
                kept.pop();
            }
            _ => {
                kept.push(segment);
                continue;
            }
        }
        // A path ending in a dot segment names the directory it leaves.
        if segments.peek().is_none() {
            kept.push("");
        }
    }
    format!("/{}", kept.join("/"))
}

/// The pin an `integrity` key's `value` writes, or why it writes none, worded
/// to follow the key's name.
///
/// # Errors
///
/// A value that is not a string, or not a pin in [`Integrity::FORM`].
pub fn pin_of(value: &AttrValue) -> Result<Integrity, String> {
    let text = value
        .as_str()
        .ok_or_else(|| format!("must be a string, got {}", value.kind()))?;
    Integrity::parse(text).ok_or_else(|| format!("is {}, got `{text}`", Integrity::FORM))
}

/// The next of `pending` to read. Each one `unpinned` says no pin holds yet
/// waits in `held` until `pending` drains, as any module read meanwhile may
/// write the pin its read is held to.
pub fn next_in_pin_order<T>(
    pending: &mut VecDeque<T>,
    held: &mut VecDeque<T>,
    unpinned: impl Fn(&T) -> bool,
) -> Option<T> {
    while let Some(next) = pending.pop_front() {
        if !unpinned(&next) {
            return Some(next);
        }
        held.push_back(next);
    }
    held.pop_front()
}

/// The SHA-256 digest a `with { integrity: "sha256:<hex>" }` pins a remote file
/// to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Integrity([u8; 32]);

impl Integrity {
    /// How a pin is written, as an error about a malformed one says.
    pub const FORM: &str = "`sha256:` and 64 lowercase hex digits";

    /// The digest of `bytes`.
    #[must_use]
    pub fn of(bytes: &[u8]) -> Self {
        Self(Sha256::digest(bytes).into())
    }

    /// The pin `text` writes in [`Self::FORM`], or `None`.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let hex = text.strip_prefix(ALGORITHM)?;
        if hex.len() != 64 {
            return None;
        }
        let mut digest = [0u8; 32];
        for (byte, pair) in digest.iter_mut().zip(hex.as_bytes().chunks(2)) {
            *byte = (hex_value(pair[0])? << 4) | hex_value(pair[1])?;
        }
        Some(Self(digest))
    }

    /// The digest's lowercase hex digits, without the algorithm.
    #[must_use]
    pub fn hex(&self) -> String {
        self.0.iter().fold(String::with_capacity(64), |mut hex, b| {
            let _ = write!(hex, "{b:02x}");
            hex
        })
    }
}

fn hex_value(digit: u8) -> Option<u8> {
    match digit {
        b'0'..=b'9' => Some(digit - b'0'),
        b'a'..=b'f' => Some(digit - b'a' + 10),
        _ => None,
    }
}

impl fmt::Display for Integrity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{ALGORITHM}{}", self.hex())
    }
}

/// Who pinned a remote file, as an integrity error names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinnedBy {
    /// The `integrity` the import site wrote.
    Use,
    /// The entry `wado.lock` records for the URL.
    Lock,
}

impl fmt::Display for PinnedBy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Use => "the `use`",
            Self::Lock => "`wado.lock`",
        })
    }
}

/// A remote file that hashes to something other than its pin.
#[derive(Debug, Clone)]
pub struct PinMismatch {
    pub url: String,
    pub expected: Integrity,
    pub actual: Integrity,
    pub pinned_by: PinnedBy,
}

/// Check that `actual`, the digest of what was fetched from `url`, is `pin`:
/// what every host does once it knows what pins the file.
///
/// # Errors
///
/// [`SourceError::IntegrityMismatch`] where `actual` is not `pin`.
pub fn check_digest(
    url: &str,
    actual: Integrity,
    pin: Integrity,
    pinned_by: PinnedBy,
) -> Result<(), SourceError> {
    if actual != pin {
        return Err(SourceError::IntegrityMismatch(Box::new(PinMismatch {
            url: url.to_string(),
            expected: pin,
            actual,
            pinned_by,
        })));
    }
    Ok(())
}

/// `bytes`, fetched from `url`, held to the `integrity` the import site wrote:
/// what a host with no lock file answers, so it refuses a file left unpinned.
///
/// # Errors
///
/// [`SourceError::IntegrityMismatch`] where they hash to anything else, and
/// [`SourceError::Unpinned`] where the site wrote no pin.
pub fn held_to_use(
    url: &str,
    bytes: Vec<u8>,
    integrity: Option<Integrity>,
) -> Result<Vec<u8>, SourceError> {
    let actual = Integrity::of(&bytes);
    let Some(pin) = integrity else {
        return Err(SourceError::Unpinned {
            url: url.to_string(),
            actual,
        });
    };
    check_digest(url, actual, pin, PinnedBy::Use)?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integrity_round_trips_through_its_text() {
        let digest = Integrity::of(b"wado");
        assert_eq!(Integrity::parse(&digest.to_string()), Some(digest));
    }

    #[test]
    fn integrity_takes_sha256_and_lowercase_hex_only() {
        let hex = Integrity::of(b"").hex();
        assert!(Integrity::parse(&format!("sha512:{hex}")).is_none());
        assert!(Integrity::parse(&format!("sha256:{}", hex.to_uppercase())).is_none());
        assert!(Integrity::parse(&format!("sha256:{}", &hex[1..])).is_none());
    }

    #[test]
    fn a_url_reference_keeps_empty_segments_and_leaves_the_query() {
        let resolve = |relative| resolve_url("https://h.test/lib//a.wado?v=/x#top", relative);
        assert_eq!(resolve("./b.wado"), "https://h.test/lib//b.wado");
        assert_eq!(resolve("../b.wado"), "https://h.test/lib/b.wado");
        assert_eq!(resolve("../../../b.wado"), "https://h.test/b.wado");
        assert_eq!(resolve("./d/../"), "https://h.test/lib//");
        assert_eq!(resolve("/b.wado?q"), "https://h.test/b.wado?q");
        assert_eq!(resolve_url("https://h.test", "./b.wado"), "https://h.test/b.wado");
    }

    #[test]
    fn plain_http_reaches_only_this_machine() {
        assert_eq!(url_fault("https://example.com/a.wado"), None);
        assert_eq!(url_fault("http://localhost:8080/a.wado"), None);
        assert_eq!(url_fault("http://127.0.0.1/a.wado"), None);
        assert_eq!(url_fault("http://[::1]:9/a.wado"), None);
        assert!(url_fault("http://example.com/a.wado").is_some());
        assert!(url_fault("http://localhost.example.com/a.wado").is_some());
    }
}
