//! Embed `[package]` metadata into a compiled component.
//!
//! The mapping from manifest fields to `(section name, payload)` pairs lives in
//! [`wado_manifest::metadata_sections`] (pure data). This module appends those
//! pairs to the component as custom sections — the `wasm-metadata` custom-section
//! format `wkg` reads — mirroring the additive WIT `component-type` embed.

use std::borrow::Cow;
use std::path::Path;
use std::process::Command;

use wado_manifest::MetadataSection;
use wasm_encoder::{Encode, Section};

/// Append each metadata section to `component` as a custom section. Returns the
/// component unchanged when there is nothing to embed.
#[must_use]
pub fn embed_metadata_sections(mut component: Vec<u8>, sections: &[MetadataSection]) -> Vec<u8> {
    for s in sections {
        let section = wasm_encoder::CustomSection {
            name: Cow::Borrowed(&s.name),
            data: Cow::Borrowed(s.value.as_bytes()),
        };
        component.push(section.id());
        section.encode(&mut component);
    }
    component
}

/// The git commit SHA at `dir` (`HEAD`), but only when the working tree is
/// clean. Returns `None` when `dir` is not a git repo, git is unavailable, or
/// the tree has uncommitted tracked changes — so a dirty build silently omits
/// the `revision` field (a warning is the `wado publish` path's job). Untracked
/// files are ignored, matching what the commit actually captures.
#[must_use]
pub fn clean_git_revision(dir: &Path) -> Option<String> {
    let status = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["status", "--porcelain", "--untracked-files=no"])
        .output()
        .ok()?;
    if !status.status.success() || !status.stdout.is_empty() {
        return None;
    }
    let rev = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()?;
    if !rev.status.success() {
        return None;
    }
    let sha = String::from_utf8(rev.stdout).ok()?.trim().to_string();
    (!sha.is_empty()).then_some(sha)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal valid empty component (just the preamble).
    fn empty_component() -> Vec<u8> {
        wasm_encoder::Component::new().finish()
    }

    fn sections_of(bytes: &[u8]) -> Vec<(String, Vec<u8>)> {
        let mut out = Vec::new();
        for payload in wasmparser::Parser::new(0).parse_all(bytes) {
            if let Ok(wasmparser::Payload::CustomSection(reader)) = payload {
                out.push((reader.name().to_string(), reader.data().to_vec()));
            }
        }
        out
    }

    #[test]
    fn embeds_each_section_with_utf8_payload() {
        let sections = vec![
            MetadataSection {
                name: "description".to_string(),
                value: "A toolkit".to_string(),
            },
            MetadataSection {
                name: "org.wado-lang.package.repository-directory".to_string(),
                value: "packages/app".to_string(),
            },
        ];
        let out = embed_metadata_sections(empty_component(), &sections);
        let found = sections_of(&out);
        assert_eq!(
            found,
            vec![
                ("description".to_string(), b"A toolkit".to_vec()),
                (
                    "org.wado-lang.package.repository-directory".to_string(),
                    b"packages/app".to_vec()
                ),
            ]
        );
    }

    #[test]
    fn empty_sections_leave_component_unchanged() {
        let component = empty_component();
        let out = embed_metadata_sections(component.clone(), &[]);
        assert_eq!(out, component);
    }
}
