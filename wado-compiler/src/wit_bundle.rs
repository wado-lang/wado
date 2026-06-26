//! Embed a `component-type` custom section into a compiled component.
//!
//! This is the producer-side embedding step of WIT interoperability (WEP
//! `wep-2026-05-02-wit-interoperability.md`, Phase 2; format from
//! `wep-2026-03-21-wit-bundling.md`). [`wit_emit`](crate::wit_emit) produces the
//! WIT *text*; this module turns that text into the binary `component-type`
//! section and appends it to the component the codegen already emitted.
//!
//! ## What the section is, and what it is not
//!
//! A Wado-compiled artifact is a *full Component Model component*, not a core
//! module. Such a component is already self-describing: its typed CM
//! imports/exports let `wasm-tools component wit` (i.e. `wit_parser::decode`)
//! reconstruct WIT directly via its `decode_component` path, with no custom
//! section involved. `wit_parser::decode` only consults a `component-type`
//! payload when the *whole file* is a WIT-package blob (the wit-bindgen → core
//! module → `wasm-tools component new` flow); a `component-type` custom section
//! on an already-formed component is opaquely carried, never read by
//! `component wit`.
//!
//! The embedded section is therefore *additive metadata*, not the mechanism
//! that makes the component inspectable. Its value: the component's intrinsic
//! type is always tree-shaken to the used surface, whereas the embedded payload
//! carries the **full-fidelity contract** — complete upstream interface bodies,
//! exact package versions, and a `producers` record — matching the
//! `wit_component::metadata::encode` toolchain convention so `wkg`,
//! `wasm-tools metadata`, and relink flows see a faithful, full-scope WIT.

use std::borrow::Cow;

use wasm_encoder::{Encode, Section};
use wit_component::StringEncoding;

use crate::semantics::Semantics;
use crate::wit_emit::{self, WitEmitError, WitEmitOptions, WitScope};

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
    opts: &WitEmitOptions,
) -> Result<Vec<u8>, WitEmitError> {
    let payload = encode_component_type(sem, opts)?;

    let section = wasm_encoder::CustomSection {
        name: "component-type".into(),
        data: Cow::Borrowed(&payload),
    };
    let mut out = Vec::with_capacity(component_bytes.len() + payload.len() + 16);
    out.extend_from_slice(component_bytes);
    out.push(section.id());
    section.encode(&mut out);
    Ok(out)
}

/// Render the WIT for `sem`/`opts` and encode it as a `component-type` section
/// payload (a serialized WIT-package component plus the string-encoding and
/// producers subsections). Exposed for tests that assert the payload decodes as
/// a standalone WIT package.
pub fn encode_component_type(
    sem: &Semantics,
    opts: &WitEmitOptions,
) -> Result<Vec<u8>, WitEmitError> {
    // Force full scope: the section must be a self-contained WIT package so
    // `metadata::encode` can type the world without an external registry.
    let opts = WitEmitOptions {
        scope: WitScope::Full,
        ..opts.clone()
    };
    let text = wit_emit::emit_wit_text(sem, &opts)?;

    let mut resolve = wit_parser::Resolve::new();
    let pkg = resolve
        .push_str("wado-embedded.wit", &text)
        .map_err(|e| WitEmitError::Embed {
            description: format!("re-parsing emitted WIT failed: {e}"),
        })?;

    let world_name = wit_emit::world_name(&opts);
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
