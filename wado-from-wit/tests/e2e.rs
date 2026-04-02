//! E2E tests for wado-from-wit

use wado_from_wit::{Transformer, WadoCodeGenerator};
use wit_parser::Resolve;

fn parse_wit_and_generate(wit: &str) -> String {
    let mut resolve = Resolve::default();
    resolve
        .push_str("<test>", wit)
        .expect("Failed to parse WIT");

    let transformer = Transformer::new(&resolve);
    let mut generator = WadoCodeGenerator::new();

    let mut output = String::new();
    for (iface_id, _) in resolve.interfaces.iter() {
        let module = transformer
            .transform_interface(iface_id)
            .expect("Failed to transform interface");
        output.push_str(&generator.generate(&module));
    }
    output
}

#[test]
fn test_simple_function() {
    let wit = r#"
package test:example@0.1.0;

interface greet {
    hello: func(name: string) -> string;
}
"#;

    let output = parse_wit_and_generate(wit);

    assert!(output.contains("pub effect Greet {"));
    assert!(output.contains("fn hello(name: String) -> String;"));
    assert!(output.contains(r#"#[cm("test:example/greet@0.1.0")]"#));
}

#[test]
fn test_record_type() {
    let wit = r#"
package test:example@0.1.0;

interface types {
    record point {
        x: s32,
        y: s32,
    }
}
"#;

    let output = parse_wit_and_generate(wit);

    assert!(output.contains("pub struct Point {"));
    assert!(output.contains("x: i32,"));
    assert!(output.contains("y: i32,"));
}

#[test]
fn test_enum_type() {
    let wit = r#"
package test:example@0.1.0;

interface types {
    enum color {
        red,
        green,
        blue,
    }
}
"#;

    let output = parse_wit_and_generate(wit);

    assert!(output.contains("pub enum Color {"));
    assert!(output.contains("Red,"));
    assert!(output.contains("Green,"));
    assert!(output.contains("Blue,"));
}

#[test]
fn test_option_and_result_types() {
    let wit = r#"
package test:example@0.1.0;

interface api {
    get-value: func(key: string) -> option<string>;
    parse: func(input: string) -> result<u32, string>;
}
"#;

    let output = parse_wit_and_generate(wit);

    assert!(output.contains("fn get_value(key: String) -> Option<String>;"));
    assert!(output.contains("fn parse(input: String) -> Result<u32, String>;"));
}

#[test]
fn test_resource_type() {
    let wit = r#"
package test:example@0.1.0;

interface storage {
    resource file {
        read: func(len: u32) -> list<u8>;
        write: func(data: list<u8>);
    }
}
"#;

    let output = parse_wit_and_generate(wit);

    assert!(output.contains("pub resource File {"));
    assert!(output.contains("fn read(self: &File, len: u32) -> Array<u8>;"));
    assert!(output.contains("fn write(self: &File, data: Array<u8>);"));
}
