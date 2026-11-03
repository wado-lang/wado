use wado_lsp::scip::{Occurrence, ScipDocument, SymbolInformation, SymbolKind, encode_index};

#[test]
fn encodes_scip_metadata_occurrences_and_symbol_information() {
    let document = ScipDocument {
        relative_path: "a.wado".into(),
        occurrences: vec![Occurrence {
            range: vec![127, 128, 300],
            symbol: "s".into(),
            roles: 9,
            enclosing_range: Some(vec![127, 0, 129, 0]),
        }],
        symbols: vec![SymbolInformation {
            symbol: "s".into(),
            kind: SymbolKind::Function,
            display_name: "f".into(),
            signature: Some("fn f()".into()),
            enclosing_symbol: "t".into(),
        }],
    };

    // Serialized SCIP fields, including packed multi-byte position varints.
    let expected = [
        0x0a, 0x19, 0x12, 0x09, 0x0a, 0x04, b'w', b'a', b'd', b'o', 0x12, 0x01, b'v',
        0x1a, 0x0a, b'f', b'i', b'l', b'e', b':', b'/', b'/', b'/', b'r', b'/', 0x20,
        0x01, 0x12, 0x42, 0x0a, 0x06, b'a', b'.', b'w', b'a', b'd', b'o', 0x12, 0x13,
        0x0a, 0x05, 0x7f, 0x80, 0x01, 0xac, 0x02, 0x12, 0x01, b's', 0x18, 0x09, 0x3a,
        0x05, 0x7f, 0x00, 0x81, 0x01, 0x00, 0x1a, 0x1b, 0x0a, 0x01, b's', 0x28, 0x11, 0x32, 0x01,
        b'f', 0x3a, 0x0e, 0x22, 0x04, b'w', b'a', b'd', b'o', 0x2a, 0x06, b'f', b'n',
        b' ', b'f', b'(', b')', 0x42, 0x01, b't', 0x22, 0x04, b'w', b'a', b'd', b'o',
        0x30, 0x03,
    ];
    assert_eq!(encode_index("file:///r/", "v", &[document]), expected);
}
