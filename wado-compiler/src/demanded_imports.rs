//! The `org.wado-lang.demanded-imports` custom section of a library component:
//! the `wasi:*` interfaces its exports demand of a caller. A host import
//! outside it is one only an `#[ambient]` body reaches, which a consumer
//! forwards without demanding its effect, as it would if it had the library as
//! source. A component without a section this module reads, one Wado did not
//! build among them, demands every import it has.
//!
//! The payload is lines: the format version, then the interface FQs. Writer
//! and reader both depend on this module, so the wire format lives in one
//! place.

use wasmparser::{Chunk, Parser, Payload};

/// Name of the custom section a library component carries.
pub const SECTION_NAME: &str = "org.wado-lang.demanded-imports";

/// The payload's first line. A reader takes no other, so a change of the
/// format bumps it, and a component of another version demands every import.
const FORMAT_VERSION: &str = "1";

/// Encode interface FQs into the section payload.
pub fn encode<'a>(fqs: impl IntoIterator<Item = &'a str>) -> Vec<u8> {
    std::iter::once(FORMAT_VERSION)
        .chain(fqs)
        .collect::<Vec<_>>()
        .join("\n")
        .into_bytes()
}

/// The section of the component `bytes` itself, not of a component or module
/// nested in it, which says what that one demands. `None` where it carries
/// none, or one of another format version or not UTF-8, which demands every
/// import.
pub fn read_from_component(mut bytes: &[u8]) -> Option<Vec<String>> {
    let mut parser = Parser::new(0);
    loop {
        let Chunk::Parsed { consumed, payload } = parser.parse(bytes, true).ok()? else {
            unreachable!("a parse at the end of its input needs no more data");
        };
        bytes = &bytes[consumed..];
        match payload {
            // The parser stepped over a nested section, so its bytes are
            // skipped here unread.
            Payload::ModuleSection {
                unchecked_range, ..
            }
            | Payload::ComponentSection {
                unchecked_range, ..
            } => {
                let len = usize::try_from(unchecked_range.end - unchecked_range.start).ok()?;
                bytes = bytes.get(len..)?;
            }
            Payload::CustomSection(reader) if reader.name() == SECTION_NAME => {
                let mut lines = std::str::from_utf8(reader.data()).ok()?.split('\n');
                if lines.next() != Some(FORMAT_VERSION) {
                    return None;
                }
                return Some(lines.map(str::to_string).collect());
            }
            Payload::End(_) => return None,
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn component_with(data: &[u8]) -> Vec<u8> {
        let mut component = wasm_encoder::Component::new();
        component.section(&wasm_encoder::CustomSection {
            name: SECTION_NAME.into(),
            data: data.into(),
        });
        component.finish()
    }

    #[test]
    fn round_trips_through_a_component() {
        let bytes = component_with(&encode([
            "wasi:cli/stderr@0.3.0",
            "wasi:clocks/system-clock@0.3.0",
        ]));
        assert_eq!(
            read_from_component(&bytes),
            Some(vec![
                "wasi:cli/stderr@0.3.0".to_string(),
                "wasi:clocks/system-clock@0.3.0".to_string(),
            ])
        );
    }

    #[test]
    fn an_empty_section_demands_nothing() {
        assert_eq!(
            read_from_component(&component_with(&encode([]))),
            Some(Vec::new())
        );
    }

    #[test]
    fn another_format_version_is_no_record() {
        assert_eq!(
            read_from_component(&component_with(b"2\nwasi:cli/stderr@0.3.0")),
            None
        );
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
        outer = wasm_encoder::Component::new();
        outer.section(&wasm_encoder::NestedComponentSection(&inner));
        outer.section(&wasm_encoder::CustomSection {
            name: SECTION_NAME.into(),
            data: encode([]).into(),
        });
        assert_eq!(read_from_component(&outer.finish()), Some(Vec::new()));
    }
}
