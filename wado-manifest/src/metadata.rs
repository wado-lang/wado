//! Mapping from `[package]` metadata to embedded component metadata sections.
//!
//! Pure data: produces the `(section name, payload)` pairs the CLI appends to
//! the component as custom sections. The encoding is the `wasm-metadata`
//! custom-section format — one custom section per field, the section name being
//! the field's bare name and the payload its UTF-8 value — so `wkg` can promote
//! the standard fields to OCI annotations on publish. No wasm or IO
//! dependencies, so it builds for `wasm32-unknown-unknown` like the rest of this
//! crate.
//!
//! Standard fields use the bare `wasm-metadata` section names (`description`,
//! `authors`, …); metadata with no standard OCI key uses the Wado namespace
//! `org.wado-lang.package.*`.

use crate::manifest::Package;

/// Custom section name for the Wado-custom monorepo subdirectory field.
pub const REPOSITORY_DIRECTORY_SECTION: &str = "org.wado-lang.package.repository-directory";

/// A custom section carrying one metadata field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetadataSection {
    /// Custom section name.
    pub name: String,
    /// UTF-8 payload.
    pub value: String,
}

impl MetadataSection {
    fn new(name: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            value: value.into(),
        }
    }
}

/// The metadata sections to embed, in deterministic order. `revision` (the git
/// commit SHA) is supplied by the caller since it is derived at build time;
/// pass `None` to omit it. Absent optional fields are skipped.
#[must_use]
pub fn metadata_sections(pkg: &Package, revision: Option<&str>) -> Vec<MetadataSection> {
    let mut out = Vec::new();
    if let Some(description) = &pkg.description {
        out.push(MetadataSection::new("description", description));
    }
    if !pkg.authors.is_empty() {
        out.push(MetadataSection::new("authors", pkg.authors.join(", ")));
    }
    if let Some(homepage) = pkg.effective_homepage() {
        out.push(MetadataSection::new("homepage", homepage));
    }
    if let Some(source) = &pkg.repository {
        out.push(MetadataSection::new("source", source));
    }
    if let Some(documentation) = pkg.effective_documentation() {
        out.push(MetadataSection::new("documentation", documentation));
    }
    if let Some(licenses) = license_expression(pkg) {
        out.push(MetadataSection::new("licenses", licenses));
    }
    out.push(MetadataSection::new("version", &pkg.version));
    if let Some(revision) = revision {
        out.push(MetadataSection::new("revision", revision));
    }
    if let Some(dir) = &pkg.repository_directory {
        out.push(MetadataSection::new(REPOSITORY_DIRECTORY_SECTION, dir));
    }
    out
}

/// The `licenses` value: the SPDX expression when `license` is set, otherwise a
/// `LicenseRef-<id>` reference for a `license-file`. `None` when neither is set.
fn license_expression(pkg: &Package) -> Option<String> {
    if let Some(license) = &pkg.license {
        return Some(license.clone());
    }
    pkg.license_file
        .as_deref()
        .map(|file| format!("LicenseRef-{}", license_ref_id(file)))
}

/// Derive an SPDX `LicenseRef` idstring from a license-file path: the file's
/// base name with any character outside `[A-Za-z0-9.-]` replaced by `-`.
#[must_use]
pub fn license_ref_id(license_file: &str) -> String {
    let base = license_file
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(license_file);
    base.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn package(toml: &str) -> Package {
        toml.parse::<crate::Manifest>().unwrap().package.unwrap()
    }

    fn value_of<'a>(sections: &'a [MetadataSection], name: &str) -> Option<&'a str> {
        sections
            .iter()
            .find(|s| s.name == name)
            .map(|s| s.value.as_str())
    }

    #[test]
    fn maps_standard_fields_to_bare_section_names() {
        let pkg = package(
            r#"
[package]
namespace = "myorg"
name = "app"
version = "0.1.0"
description = "A fast widget toolkit"
homepage = "https://wado-lang.org"
repository = "https://github.com/myorg/app"
documentation = "https://docs.wado-lang.org"
license = "MIT OR Apache-2.0"
authors = ["Alice <alice@example.com>", "Bob"]
"#,
        );
        let sections = metadata_sections(&pkg, None);
        assert_eq!(
            value_of(&sections, "description"),
            Some("A fast widget toolkit")
        );
        assert_eq!(value_of(&sections, "homepage"), Some("https://wado-lang.org"));
        assert_eq!(
            value_of(&sections, "source"),
            Some("https://github.com/myorg/app")
        );
        assert_eq!(
            value_of(&sections, "documentation"),
            Some("https://docs.wado-lang.org")
        );
        assert_eq!(value_of(&sections, "licenses"), Some("MIT OR Apache-2.0"));
        assert_eq!(
            value_of(&sections, "authors"),
            Some("Alice <alice@example.com>, Bob")
        );
        assert_eq!(value_of(&sections, "version"), Some("0.1.0"));
        // No git SHA passed.
        assert!(value_of(&sections, "revision").is_none());
    }

    #[test]
    fn homepage_and_documentation_fall_back_to_repository() {
        let pkg = package(
            r#"
[package]
name = "app"
version = "0.1.0"
repository = "https://github.com/myorg/app"
"#,
        );
        let sections = metadata_sections(&pkg, None);
        assert_eq!(
            value_of(&sections, "homepage"),
            Some("https://github.com/myorg/app")
        );
        assert_eq!(
            value_of(&sections, "documentation"),
            Some("https://github.com/myorg/app")
        );
    }

    #[test]
    fn absent_optional_fields_are_skipped() {
        let pkg = package("[package]\nname = \"app\"\nversion = \"0.1.0\"\n");
        let sections = metadata_sections(&pkg, None);
        assert!(value_of(&sections, "description").is_none());
        assert!(value_of(&sections, "homepage").is_none());
        assert!(value_of(&sections, "licenses").is_none());
        // version is always present.
        assert_eq!(value_of(&sections, "version"), Some("0.1.0"));
    }

    #[test]
    fn revision_is_included_when_supplied() {
        let pkg = package("[package]\nname = \"app\"\nversion = \"0.1.0\"\n");
        let sections = metadata_sections(&pkg, Some("abc1234def5678"));
        assert_eq!(value_of(&sections, "revision"), Some("abc1234def5678"));
    }

    #[test]
    fn repository_directory_uses_wado_namespace() {
        let pkg = package(
            r#"
[package]
name = "app"
version = "0.1.0"
repository-directory = "packages/app"
"#,
        );
        let sections = metadata_sections(&pkg, None);
        assert_eq!(
            value_of(&sections, "org.wado-lang.package.repository-directory"),
            Some("packages/app")
        );
    }

    #[test]
    fn license_file_becomes_license_ref() {
        let pkg = package(
            r#"
[package]
name = "app"
version = "0.1.0"
license-file = "licenses/LICENSE-COMMERCIAL.txt"
"#,
        );
        let sections = metadata_sections(&pkg, None);
        assert_eq!(
            value_of(&sections, "licenses"),
            Some("LicenseRef-LICENSE-COMMERCIAL.txt")
        );
    }

    #[test]
    fn license_ref_id_sanitizes_invalid_chars() {
        assert_eq!(license_ref_id("LICENSE-COMMERCIAL"), "LICENSE-COMMERCIAL");
        assert_eq!(license_ref_id("dir/My License.md"), "My-License.md");
    }
}
