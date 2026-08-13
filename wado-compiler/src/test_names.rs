//! Encoding of the `org.wado-lang.test-names` custom section. A CM export name
//! must be ASCII kebab-case, so `test-0-hello-world` is a lossy rendering of
//! `test "Hello, World!"`; codegen writes this section mapping each export name
//! back to the faithful one for `wado test` to label results with. Writer and
//! reader both depend on this module, so the wire format lives in one place.
//!
//! Integers are little-endian `u32` and strings length-prefixed UTF-8: a `count`
//! followed by that many `(export_name, has_name, name)` records, where a `0`
//! `has_name` marks an unnamed test and leaves `name_len` zero.

/// Name of the custom section embedded in test-world components.
pub const SECTION_NAME: &str = "org.wado-lang.test-names";

/// Encode `(export_name, original_name)` pairs into the section payload.
pub fn encode<'a>(entries: impl IntoIterator<Item = (&'a str, Option<&'a str>)>) -> Vec<u8> {
    let entries: Vec<(&str, Option<&str>)> = entries.into_iter().collect();
    let mut out = Vec::new();
    out.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    for (export_name, original_name) in entries {
        push_str(&mut out, export_name);
        if let Some(name) = original_name {
            out.push(1);
            push_str(&mut out, name);
        } else {
            out.push(0);
            out.extend_from_slice(&0u32.to_le_bytes());
        }
    }
    out
}

/// Decode a section payload into `(export_name, original_name)` pairs.
///
/// Returns `None` if the bytes are truncated or otherwise malformed, so the
/// caller can treat a corrupt section as "no name information" rather than
/// crashing.
pub fn decode(data: &[u8]) -> Option<Vec<(String, Option<String>)>> {
    let mut cursor = Cursor { data, pos: 0 };
    let count = cursor.read_u32()? as usize;
    let mut entries = Vec::with_capacity(count);
    for _ in 0..count {
        let export_name = cursor.read_str()?;
        let has_name = cursor.read_u8()?;
        let name = cursor.read_str()?;
        let original_name = if has_name == 1 { Some(name) } else { None };
        entries.push((export_name, original_name));
    }
    Some(entries)
}

/// Find and decode the `org.wado-lang.test-names` custom section in a component binary.
///
/// Returns `None` when the section is absent or malformed. Callers that expect
/// the section (test-world components) should treat `None` as "no name
/// information available" for whichever tests they were looking up.
pub fn read_from_component(component_bytes: &[u8]) -> Option<Vec<(String, Option<String>)>> {
    for payload in wasmparser::Parser::new(0).parse_all(component_bytes) {
        if let Ok(wasmparser::Payload::CustomSection(reader)) = payload
            && reader.name() == SECTION_NAME
        {
            return decode(reader.data());
        }
    }
    None
}

fn push_str(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(&(s.len() as u32).to_le_bytes());
    out.extend_from_slice(s.as_bytes());
}

struct Cursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl Cursor<'_> {
    fn read_u8(&mut self) -> Option<u8> {
        let b = *self.data.get(self.pos)?;
        self.pos += 1;
        Some(b)
    }

    fn read_u32(&mut self) -> Option<u32> {
        let bytes = self.data.get(self.pos..self.pos + 4)?;
        self.pos += 4;
        Some(u32::from_le_bytes(bytes.try_into().ok()?))
    }

    fn read_str(&mut self) -> Option<String> {
        let len = self.read_u32()? as usize;
        let bytes = self.data.get(self.pos..self.pos + len)?;
        self.pos += len;
        String::from_utf8(bytes.to_vec()).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_named_and_unnamed() {
        let entries = [
            ("test-0-hello-world", Some("Hello, World!")),
            ("test-1", None),
            ("test-2-cafe", Some("café résumé")),
            ("test-3", Some("日本語のテスト ok")),
        ];
        let encoded = encode(entries.iter().map(|(e, n)| (*e, *n)));
        let decoded = decode(&encoded).expect("valid payload");
        let expected: Vec<(String, Option<String>)> = entries
            .iter()
            .map(|(e, n)| ((*e).to_string(), n.map(str::to_string)))
            .collect();
        assert_eq!(decoded, expected);
    }

    #[test]
    fn empty_section_round_trips() {
        let encoded = encode(std::iter::empty());
        assert_eq!(decode(&encoded), Some(vec![]));
    }

    #[test]
    fn truncated_payload_is_rejected() {
        assert_eq!(decode(&[1, 0, 0]), None); // count claims 1 but no entry follows
    }
}
