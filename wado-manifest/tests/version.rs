use wado_manifest::{Version, VersionSpecifier};

#[test]
fn version_ordering() {
    assert!(Version::parse("1.0.0").unwrap() < Version::parse("1.0.1").unwrap());
    assert!(Version::parse("1.1.0").unwrap() < Version::parse("2.0.0").unwrap());
}

#[test]
fn caret_range_boundary_cases() {
    let spec = VersionSpecifier::parse("^1.0.0").unwrap();
    assert!(spec.matches(&Version::parse("1.99.99").unwrap()));
    assert!(!spec.matches(&Version::parse("2.0.0").unwrap()));
    assert!(!spec.matches(&Version::parse("0.99.99").unwrap()));

    let spec = VersionSpecifier::parse("^0.2.0").unwrap();
    assert!(spec.matches(&Version::parse("0.2.99").unwrap()));
    assert!(!spec.matches(&Version::parse("0.3.0").unwrap()));
}

#[test]
fn tilde_range_boundary_cases() {
    let spec = VersionSpecifier::parse("~1.2.0").unwrap();
    assert!(spec.matches(&Version::parse("1.2.99").unwrap()));
    assert!(!spec.matches(&Version::parse("1.3.0").unwrap()));
}

#[test]
fn bare_version_rejected_with_message() {
    let msg = VersionSpecifier::parse("1.0.0").unwrap_err().to_string();
    assert!(msg.contains("1.0.0"));
    assert!(msg.contains("prefix"));
}
