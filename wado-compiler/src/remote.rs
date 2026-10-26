//! Remote files: a `use` or a Kiln input naming an `http://` or `https://` URL,
//! and the digest that pins what the URL serves.

use std::fmt::{self, Write};

use sha2::{Digest, Sha256};

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

/// The SHA-256 digest a `with { integrity: "sha256:<hex>" }` pins a remote file
/// to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Integrity([u8; 32]);

impl Integrity {
    /// The digest of `bytes`.
    #[must_use]
    pub fn of(bytes: &[u8]) -> Self {
        Self(Sha256::digest(bytes).into())
    }

    /// `sha256:` and 64 lowercase hex digits, or `None`.
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

/// Check that `bytes`, fetched from `url`, hash to `pin`: what every host does
/// once it knows what pins the file.
///
/// # Errors
///
/// [`SourceError::IntegrityMismatch`] where they hash to anything else.
pub fn check_pin(
    url: &str,
    bytes: &[u8],
    pin: Integrity,
    pinned_by: PinnedBy,
) -> Result<(), SourceError> {
    check_digest(url, Integrity::of(bytes), pin, pinned_by)
}

/// [`check_pin`] for a host that already hashed what it fetched.
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
    let Some(pin) = integrity else {
        return Err(SourceError::Unpinned {
            url: url.to_string(),
            actual: Integrity::of(&bytes),
        });
    };
    check_pin(url, &bytes, pin, PinnedBy::Use)?;
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
    fn plain_http_reaches_only_this_machine() {
        assert_eq!(url_fault("https://example.com/a.wado"), None);
        assert_eq!(url_fault("http://localhost:8080/a.wado"), None);
        assert_eq!(url_fault("http://127.0.0.1/a.wado"), None);
        assert_eq!(url_fault("http://[::1]:9/a.wado"), None);
        assert!(url_fault("http://example.com/a.wado").is_some());
        assert!(url_fault("http://localhost.example.com/a.wado").is_some());
    }
}
