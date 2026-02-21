use std::fmt;

/// A parsed semver version (MAJOR.MINOR.PATCH).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
}

impl Version {
    /// Parse a version string like `"1.2.3"`.
    ///
    /// # Errors
    /// Returns `VersionError` if the string is not valid semver.
    pub fn parse(s: &str) -> Result<Self, VersionError> {
        let parts: Vec<&str> = s.split('.').collect();
        if parts.len() != 3 {
            return Err(VersionError::InvalidFormat {
                input: s.to_string(),
                reason: "expected MAJOR.MINOR.PATCH format".to_string(),
            });
        }
        let major = parts[0]
            .parse::<u64>()
            .map_err(|_| VersionError::InvalidFormat {
                input: s.to_string(),
                reason: format!("invalid major version: {:?}", parts[0]),
            })?;
        let minor = parts[1]
            .parse::<u64>()
            .map_err(|_| VersionError::InvalidFormat {
                input: s.to_string(),
                reason: format!("invalid minor version: {:?}", parts[1]),
            })?;
        let patch = parts[2]
            .parse::<u64>()
            .map_err(|_| VersionError::InvalidFormat {
                input: s.to_string(),
                reason: format!("invalid patch version: {:?}", parts[2]),
            })?;
        Ok(Version {
            major,
            minor,
            patch,
        })
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// A version specifier with a range operator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VersionSpecifier {
    /// `^1.2.3` — compatible range (same major for >=1.0.0, same major.minor for 0.x).
    Caret(Version),
    /// `~1.2.3` — patch-only range (>=1.2.3, <1.3.0).
    Tilde(Version),
    /// `=1.2.3` — exact version.
    Exact(Version),
}

impl VersionSpecifier {
    /// Parse a version specifier string like `"^1.2.3"`, `"~1.2.3"`, or `"=1.2.3"`.
    ///
    /// Bare versions (without prefix) are rejected with a clear error.
    ///
    /// # Errors
    /// Returns `VersionError` if the string is not a valid specifier.
    pub fn parse(s: &str) -> Result<Self, VersionError> {
        if let Some(rest) = s.strip_prefix('^') {
            let version = Version::parse(rest)?;
            Ok(VersionSpecifier::Caret(version))
        } else if let Some(rest) = s.strip_prefix('~') {
            let version = Version::parse(rest)?;
            Ok(VersionSpecifier::Tilde(version))
        } else if let Some(rest) = s.strip_prefix('=') {
            let version = Version::parse(rest)?;
            Ok(VersionSpecifier::Exact(version))
        } else {
            Err(VersionError::BareVersion {
                input: s.to_string(),
            })
        }
    }

    /// Check whether a concrete version satisfies this specifier.
    #[must_use]
    pub fn matches(&self, version: &Version) -> bool {
        match self {
            VersionSpecifier::Caret(base) => {
                if version < base {
                    return false;
                }
                if base.major == 0 {
                    // ^0.x.y → same major.minor
                    version.major == base.major && version.minor == base.minor
                } else {
                    // ^x.y.z → same major
                    version.major == base.major
                }
            }
            VersionSpecifier::Tilde(base) => {
                if version < base {
                    return false;
                }
                // ~x.y.z → same major.minor
                version.major == base.major && version.minor == base.minor
            }
            VersionSpecifier::Exact(base) => version == base,
        }
    }

    /// Return the base version of this specifier.
    #[must_use]
    pub fn version(&self) -> &Version {
        match self {
            VersionSpecifier::Caret(v)
            | VersionSpecifier::Tilde(v)
            | VersionSpecifier::Exact(v) => v,
        }
    }
}

impl fmt::Display for VersionSpecifier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VersionSpecifier::Caret(v) => write!(f, "^{v}"),
            VersionSpecifier::Tilde(v) => write!(f, "~{v}"),
            VersionSpecifier::Exact(v) => write!(f, "={v}"),
        }
    }
}

/// Errors from version parsing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VersionError {
    /// Version string does not match MAJOR.MINOR.PATCH.
    InvalidFormat { input: String, reason: String },
    /// Version specifier is missing a prefix (`^`, `~`, or `=`).
    BareVersion { input: String },
}

impl fmt::Display for VersionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VersionError::InvalidFormat { input, reason } => {
                write!(f, "invalid version {input:?}: {reason}")
            }
            VersionError::BareVersion { input } => {
                write!(
                    f,
                    "bare version {input:?} requires explicit prefix (^, ~, or =)"
                )
            }
        }
    }
}

impl std::error::Error for VersionError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_version() {
        let v = Version::parse("1.2.3").unwrap();
        assert_eq!(
            v,
            Version {
                major: 1,
                minor: 2,
                patch: 3
            }
        );
        assert_eq!(v.to_string(), "1.2.3");
    }

    #[test]
    fn parse_version_zero() {
        let v = Version::parse("0.0.0").unwrap();
        assert_eq!(
            v,
            Version {
                major: 0,
                minor: 0,
                patch: 0
            }
        );
    }

    #[test]
    fn parse_version_invalid() {
        assert!(Version::parse("1.2").is_err());
        assert!(Version::parse("1.2.3.4").is_err());
        assert!(Version::parse("abc").is_err());
    }

    #[test]
    fn parse_specifier_caret() {
        let s = VersionSpecifier::parse("^1.2.3").unwrap();
        assert_eq!(
            s,
            VersionSpecifier::Caret(Version {
                major: 1,
                minor: 2,
                patch: 3
            })
        );
        assert_eq!(s.to_string(), "^1.2.3");
    }

    #[test]
    fn parse_specifier_tilde() {
        let s = VersionSpecifier::parse("~0.5.1").unwrap();
        assert_eq!(
            s,
            VersionSpecifier::Tilde(Version {
                major: 0,
                minor: 5,
                patch: 1
            })
        );
    }

    #[test]
    fn parse_specifier_exact() {
        let s = VersionSpecifier::parse("=2.0.0").unwrap();
        assert_eq!(
            s,
            VersionSpecifier::Exact(Version {
                major: 2,
                minor: 0,
                patch: 0
            })
        );
    }

    #[test]
    fn bare_version_rejected() {
        let err = VersionSpecifier::parse("1.0.0").unwrap_err();
        assert!(matches!(err, VersionError::BareVersion { .. }));
    }

    #[test]
    fn caret_matches() {
        let spec = VersionSpecifier::parse("^1.2.3").unwrap();
        assert!(spec.matches(&Version::parse("1.2.3").unwrap()));
        assert!(spec.matches(&Version::parse("1.9.0").unwrap()));
        assert!(!spec.matches(&Version::parse("1.2.2").unwrap()));
        assert!(!spec.matches(&Version::parse("2.0.0").unwrap()));
        assert!(!spec.matches(&Version::parse("0.9.0").unwrap()));
    }

    #[test]
    fn caret_pre_1_0() {
        let spec = VersionSpecifier::parse("^0.2.3").unwrap();
        assert!(spec.matches(&Version::parse("0.2.3").unwrap()));
        assert!(spec.matches(&Version::parse("0.2.9").unwrap()));
        assert!(!spec.matches(&Version::parse("0.3.0").unwrap()));
        assert!(!spec.matches(&Version::parse("0.2.2").unwrap()));
    }

    #[test]
    fn tilde_matches() {
        let spec = VersionSpecifier::parse("~1.2.3").unwrap();
        assert!(spec.matches(&Version::parse("1.2.3").unwrap()));
        assert!(spec.matches(&Version::parse("1.2.9").unwrap()));
        assert!(!spec.matches(&Version::parse("1.3.0").unwrap()));
        assert!(!spec.matches(&Version::parse("1.2.2").unwrap()));
    }

    #[test]
    fn exact_matches() {
        let spec = VersionSpecifier::parse("=1.2.3").unwrap();
        assert!(spec.matches(&Version::parse("1.2.3").unwrap()));
        assert!(!spec.matches(&Version::parse("1.2.4").unwrap()));
        assert!(!spec.matches(&Version::parse("1.2.2").unwrap()));
    }
}
