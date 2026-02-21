use wado_manifest::{Version, VersionError, VersionSpecifier};

#[test]
fn version_ordering() {
    let v1 = Version::parse("1.0.0").unwrap();
    let v2 = Version::parse("1.0.1").unwrap();
    let v3 = Version::parse("1.1.0").unwrap();
    let v4 = Version::parse("2.0.0").unwrap();
    assert!(v1 < v2);
    assert!(v2 < v3);
    assert!(v3 < v4);
}

#[test]
fn caret_range_boundary_cases() {
    // ^1.0.0 includes 1.99.99, excludes 2.0.0
    let spec = VersionSpecifier::parse("^1.0.0").unwrap();
    assert!(spec.matches(&Version::parse("1.99.99").unwrap()));
    assert!(!spec.matches(&Version::parse("2.0.0").unwrap()));
    assert!(!spec.matches(&Version::parse("0.99.99").unwrap()));

    // ^0.0.0 only matches 0.0.x
    let spec = VersionSpecifier::parse("^0.0.0").unwrap();
    assert!(spec.matches(&Version::parse("0.0.0").unwrap()));
    assert!(spec.matches(&Version::parse("0.0.99").unwrap()));
    assert!(!spec.matches(&Version::parse("0.1.0").unwrap()));
}

#[test]
fn tilde_range_boundary_cases() {
    // ~1.2.0 includes 1.2.99, excludes 1.3.0
    let spec = VersionSpecifier::parse("~1.2.0").unwrap();
    assert!(spec.matches(&Version::parse("1.2.0").unwrap()));
    assert!(spec.matches(&Version::parse("1.2.99").unwrap()));
    assert!(!spec.matches(&Version::parse("1.3.0").unwrap()));
    assert!(!spec.matches(&Version::parse("1.1.99").unwrap()));
}

#[test]
fn version_display() {
    let spec = VersionSpecifier::parse("^1.2.3").unwrap();
    assert_eq!(spec.to_string(), "^1.2.3");

    let spec = VersionSpecifier::parse("~0.5.0").unwrap();
    assert_eq!(spec.to_string(), "~0.5.0");

    let spec = VersionSpecifier::parse("=3.0.0").unwrap();
    assert_eq!(spec.to_string(), "=3.0.0");
}

#[test]
fn version_accessor() {
    let spec = VersionSpecifier::parse("^1.2.3").unwrap();
    assert_eq!(spec.version(), &Version::parse("1.2.3").unwrap());
}

#[test]
fn bare_version_error_message() {
    let err = VersionSpecifier::parse("1.0.0").unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("1.0.0"));
    assert!(msg.contains("prefix"));
}

#[test]
fn invalid_version_format() {
    assert!(matches!(
        Version::parse("").unwrap_err(),
        VersionError::InvalidFormat { .. }
    ));
    assert!(matches!(
        Version::parse("1").unwrap_err(),
        VersionError::InvalidFormat { .. }
    ));
    assert!(matches!(
        Version::parse("1.2").unwrap_err(),
        VersionError::InvalidFormat { .. }
    ));
    assert!(matches!(
        Version::parse("a.b.c").unwrap_err(),
        VersionError::InvalidFormat { .. }
    ));
    assert!(matches!(
        Version::parse("1.2.3.4").unwrap_err(),
        VersionError::InvalidFormat { .. }
    ));
}
