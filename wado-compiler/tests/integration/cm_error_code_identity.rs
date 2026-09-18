//! Two interfaces in one CM package, each declaring its own `error-code`.
//!
//! `wasi:sockets/types` and `wasi:sockets/ip-name-lookup` both declare one, and
//! they are unrelated variants with different cases. An outer-scope key built
//! from the package cannot name both, so whichever interface aliased first took
//! the key and the other's alias was silently dropped — leaving any consumer
//! that asked for "the sockets error-code" holding the wrong type.

use crate::common::compile_source;

const BOTH_SOCKETS_ERROR_CODES: &str = r#"
use { TcpSocket, IpAddressFamily, IpNameLookup } from "wasi:sockets";

export fn run() with (TcpSocket, IpNameLookup) {
    let socket = TcpSocket::create(IpAddressFamily::Ipv4);
    let addresses = IpNameLookup::resolve_addresses("example.com").wait();
    let _ = socket matches { Ok(_) } && addresses matches { Ok(_) };
}
"#;

#[test]
fn each_interface_aliases_its_own_error_code() {
    let result = compile_source(BOTH_SOCKETS_ERROR_CODES)
        .unwrap_or_else(|e| panic!("both error-codes of one package must compile: {e}"));
    let wat = wasmprinter::print_bytes(&result.wasm).expect("disassemble the component to WAT");

    let aliased: Vec<&str> = wat
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with("(alias export") && line.contains("\"error-code\""))
        .collect();

    for interface in [
        "wasi:sockets/types@",
        "wasi:sockets/ip-name-lookup@",
        "wasi:cli/types@",
    ] {
        assert!(
            aliased.iter().any(|line| line.contains(interface)),
            "`{interface}` declares its own error-code but aliased none of it; \
             the outer aliases are {aliased:#?}"
        );
    }
}
