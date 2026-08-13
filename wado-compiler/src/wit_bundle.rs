//! Embed a `component-type` custom section into a compiled component — the
//! producer side of WIT interoperability (WEP 2026-05-02), encoding the text
//! [`wit_emit`](crate::wit_emit) renders. Purely additive: a Wado artifact
//! already self-describes. The section's value is fidelity, carrying the
//! complete upstream interfaces where the component's own type is tree-shaken.

use std::borrow::Cow;

use wasm_encoder::{Encode, Section};
use wit_component::StringEncoding;

use crate::semantics::Semantics;
use crate::wit_emit::{self, WitEmitError, WitEmitInput, WitEmitOptions, WitScope};

/// Append a `component-type` custom section to `component_bytes`, derived from
/// the WIT text [`wit_emit::emit_wit_text`] renders for `sem`. The section is
/// always self-contained — `metadata::encode` needs a fully-resolved
/// `Resolve` — so `opts.scope` is ignored and the whole interface closure is
/// emitted. The component's own type structure is untouched.
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

/// Like [`encode_component_type`], but from a detached [`WitEmitInput`] view (issue #1654).
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
