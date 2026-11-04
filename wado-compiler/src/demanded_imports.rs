//! The `org.wado-lang.demanded-imports` custom section of a library component:
//! the `wasi:*` interfaces its exports demand of a caller. A host import
//! outside it is one only an `#[ambient]` body reaches, which a consumer
//! forwards without demanding its effect, as it would if it had the library as
//! source. A component without the section demands every import it has.
//!
//! The payload is the interface FQs, one per line. Writer and reader both
//! depend on this module, so the wire format lives in one place.

/// Name of the custom section a library component carries.
pub const SECTION_NAME: &str = "org.wado-lang.demanded-imports";

/// Encode interface FQs into the section payload.
pub fn encode<'a>(fqs: impl IntoIterator<Item = &'a str>) -> Vec<u8> {
    fqs.into_iter().collect::<Vec<_>>().join("\n").into_bytes()
}

/// The section of the component `bytes` itself, not of a component or module
/// nested in it, which says what that one demands. `None` where it carries
/// none, or one that is not UTF-8, which demands every import.
pub fn read_from_component(bytes: &[u8]) -> Option<Vec<String>> {
    let mut depth = 0usize;
    for payload in wasmparser::Parser::new(0).parse_all(bytes) {
        match payload.ok()? {
            wasmparser::Payload::ModuleSection { .. }
            | wasmparser::Payload::ComponentSection { .. } => depth += 1,
            wasmparser::Payload::End(_) if depth == 0 => break,
            wasmparser::Payload::End(_) => depth -= 1,
            wasmparser::Payload::CustomSection(reader)
                if depth == 0 && reader.name() == SECTION_NAME =>
            {
                let text = std::str::from_utf8(reader.data()).ok()?;
                return Some(
                    text.split('\n')
                        .filter(|fq| !fq.is_empty())
                        .map(str::to_string)
                        .collect(),
                );
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_a_component() {
        let mut component = wasm_encoder::Component::new();
        component.section(&wasm_encoder::CustomSection {
            name: SECTION_NAME.into(),
            data: encode(["wasi:cli/stderr@0.3.0", "wasi:clocks/system-clock@0.3.0"]).into(),
        });
        assert_eq!(
            read_from_component(&component.finish()),
            Some(vec![
                "wasi:cli/stderr@0.3.0".to_string(),
                "wasi:clocks/system-clock@0.3.0".to_string(),
            ])
        );
    }

    #[test]
    fn an_empty_section_demands_nothing() {
        let mut component = wasm_encoder::Component::new();
        component.section(&wasm_encoder::CustomSection {
            name: SECTION_NAME.into(),
            data: encode([]).into(),
        });
        assert_eq!(read_from_component(&component.finish()), Some(Vec::new()));
    }

    #[test]
    fn a_nested_component_speaks_for_itself() {
        let mut inner = wasm_encoder::Component::new();
        inner.section(&wasm_encoder::CustomSection {
            name: SECTION_NAME.into(),
            data: encode(["wasi:cli/stderr@0.3.0"]).into(),
        });
        let mut outer = wasm_encoder::Component::new();
        outer.section(&wasm_encoder::NestedComponentSection(&inner));
        assert_eq!(read_from_component(&outer.finish()), None);
    }
}
