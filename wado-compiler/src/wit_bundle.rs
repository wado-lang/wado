//! Embed a `component-type` custom section into a compiled component.
//!
//! Producer-side embedding step of WIT interoperability (WEP
//! `wep-2026-05-02-wit-interoperability.md`, Phase 2). [`wit_emit`](crate::wit_emit)
//! renders the WIT *text*; this module encodes it to the binary `component-type`
//! section and appends it to the component codegen emitted.
//!
//! The section is *additive*: a Wado artifact is already a self-describing
//! component, so `wasm-tools component wit` (`wit_parser::decode`) recovers WIT
//! from the component's own types and does not read this section (it consults a
//! `component-type` payload only for a WIT-package blob / core module). The
//! section's value is full fidelity — the component's own type is tree-shaken to
//! the used surface, whereas the encoded payload carries the complete upstream
//! interfaces, exact versions, and a `producers` record (the
//! `wit_component::metadata::encode` convention `wkg` / `wasm-tools metadata`
//! consume).

use std::borrow::Cow;

use wasm_encoder::{Encode, Section};
use wit_component::StringEncoding;

use crate::semantics::Semantics;
use crate::wit_emit::{self, WitEmitError, WitEmitInput, WitEmitOptions, WitScope};

/// Append a `component-type` custom section to `component_bytes`, derived from
/// the WIT text [`wit_emit::emit_wit_text`] renders for `sem` under `opts`.
///
/// The embedded section is always self-contained: `metadata::encode` types the
/// world against a fully-resolved [`wit_parser::Resolve`], which requires every
/// referenced upstream package's body to be present. `opts.scope` is therefore
/// ignored here and the full interface closure is always emitted — a `local`
/// (registry-referencing) document does not re-parse standalone. `local` scope
/// remains meaningful only for `wado wit` *text*. The returned bytes are the
/// original component with one extra custom section — the component's own type
/// structure (what `wasm-tools component wit` reads) is untouched, so the
/// artifact still decodes as a `Component`.
pub fn embed_component_type(
    component_bytes: &[u8],
    sem: &Semantics,
    world_imports: &[String],
) -> Result<Vec<u8>, WitEmitError> {
    let payload = encode_component_type(sem, world_imports)?;
    Ok(append_component_type_section(component_bytes, &payload))
}

/// Like [`embed_component_type`], but from a detached [`WitEmitInput`] view
/// (issue #1654).
pub fn embed_component_type_from(
    component_bytes: &[u8],
    input: WitEmitInput<'_>,
    world_imports: &[String],
) -> Result<Vec<u8>, WitEmitError> {
    let payload = encode_component_type_from(input, world_imports)?;
    Ok(append_component_type_section(component_bytes, &payload))
}

/// Append an already-encoded `component-type` payload to a component as a custom
/// section, leaving the component's own type structure untouched.
#[must_use]
pub fn append_component_type_section(component_bytes: &[u8], payload: &[u8]) -> Vec<u8> {
    let section = wasm_encoder::CustomSection {
        name: "component-type".into(),
        data: Cow::Borrowed(payload),
    };
    let mut out = Vec::with_capacity(component_bytes.len() + payload.len() + 16);
    out.extend_from_slice(component_bytes);
    out.push(section.id());
    section.encode(&mut out);
    out
}

/// Render the WIT for `sem`/`opts` and encode it as a `component-type` section
/// payload (a serialized WIT-package component plus the string-encoding and
/// producers subsections). Exposed for tests that assert the payload decodes as
/// a standalone WIT package.
pub fn encode_component_type(
    sem: &Semantics,
    world_imports: &[String],
) -> Result<Vec<u8>, WitEmitError> {
    encode_component_type_from(sem.wit_emit_input(), world_imports)
}

/// Like [`encode_component_type`], but from a detached [`WitEmitInput`] view, so
/// the `wado compile` embed path encodes the section from the main compile's
/// retained subset without a second frontend analysis (issue #1654).
pub fn encode_component_type_from(
    input: WitEmitInput<'_>,
    world_imports: &[String],
) -> Result<Vec<u8>, WitEmitError> {
    // Force full scope: the section must be a self-contained WIT package so
    // `metadata::encode` can type the world without an external registry.
    let opts = WitEmitOptions {
        scope: WitScope::Full,
    };
    let text = wit_emit::emit_wit_text_from(input, &opts, world_imports)?;

    let mut resolve = wit_parser::Resolve::new();
    let pkg = resolve
        .push_str("wado-embedded.wit", &text)
        .map_err(|e| WitEmitError::Embed {
            description: format!("re-parsing emitted WIT failed: {e}"),
        })?;

    let world_name = wit_emit::world_name_from(input);
    let world = resolve
        .select_world(&[pkg], Some(world_name.as_str()))
        .map_err(|e| WitEmitError::Embed {
            description: format!("selecting world `{world_name}` failed: {e}"),
        })?;

    // UTF-8 matches Wado's native string encoding (WEP: WIT Bundling §"What Is
    // Embedded"). `None` extra producers — `metadata::encode` writes its own
    // `processed-by` record.
    wit_component::metadata::encode(&resolve, world, StringEncoding::UTF8, None).map_err(|e| {
        WitEmitError::Embed {
            description: format!("encoding component-type metadata failed: {e}"),
        }
    })
}
