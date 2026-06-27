use std::fmt;

pub use semver::Version;

/// A version requirement. Only the `^`/`~`/`=` forms are accepted; a bare
/// version is rejected (the WEP requires an explicit operator).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionSpecifier(semver::VersionReq);

impl VersionSpecifier {
    /// # Errors
    /// `VersionError::BareVersion` for a missing operator, `InvalidFormat` otherwise.
    pub fn parse(s: &str) -> Result<Self, VersionError> {
        if !matches!(s.chars().next(), Some('^' | '~' | '=')) {
            return Err(VersionError::BareVersion {
                input: s.to_string(),
            });
        }
        let req = semver::VersionReq::parse(s).map_err(|e| VersionError::InvalidFormat {
            input: s.to_string(),
            reason: e.to_string(),
        })?;
        Ok(Self(req))
    }

    #[must_use]
    pub fn matches(&self, version: &Version) -> bool {
        self.0.matches(version)
    }
}

impl fmt::Display for VersionSpecifier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VersionError {
    InvalidFormat { input: String, reason: String },
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

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    #[test]
    fn bare_version_rejected() {
        assert!(matches!(
            VersionSpecifier::parse("1.0.0"),
            Err(VersionError::BareVersion { .. })
        ));
    }

    #[test]
    fn invalid_specifier_rejected() {
        assert!(matches!(
            VersionSpecifier::parse("^not.a.version"),
            Err(VersionError::InvalidFormat { .. })
        ));
    }

    #[test]
    fn caret_matches() {
        let s = VersionSpecifier::parse("^1.2.3").unwrap();
        assert!(s.matches(&v("1.2.3")));
        assert!(s.matches(&v("1.9.0")));
        assert!(!s.matches(&v("1.2.2")));
        assert!(!s.matches(&v("2.0.0")));
        assert!(!s.matches(&v("0.9.0")));
    }

    #[test]
    fn caret_pre_1_0() {
        let s = VersionSpecifier::parse("^0.2.3").unwrap();
        assert!(s.matches(&v("0.2.9")));
        assert!(!s.matches(&v("0.3.0")));
        assert!(!s.matches(&v("0.2.2")));
    }

    #[test]
    fn tilde_matches() {
        let s = VersionSpecifier::parse("~1.2.3").unwrap();
        assert!(s.matches(&v("1.2.9")));
        assert!(!s.matches(&v("1.3.0")));
        assert!(!s.matches(&v("1.2.2")));
    }

    #[test]
    fn exact_matches() {
        let s = VersionSpecifier::parse("=1.2.3").unwrap();
        assert!(s.matches(&v("1.2.3")));
        assert!(!s.matches(&v("1.2.4")));
    }
}
