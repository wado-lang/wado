# WEP: URL Standard Library (`core:url`)

## Context

Wado programs frequently work with URLs — HTTP clients construct request URLs, HTTP servers parse incoming request paths, and configuration strings embed connection endpoints. Currently, `wasi:http` decomposes URLs into three string fragments (`scheme`, `authority`, `path_with_query`) with no structured parsing, validation, or manipulation. Users must manually split, join, and percent-encode these strings, which is error-prone and repetitive.

### Design Goals

1. **WHATWG URL aligned**: Follow the WHATWG URL Living Standard for special schemes (`http`, `https`, `ws`, `wss`), giving users the same URL behavior they expect from browsers and modern runtimes
2. **Minimal Option wrapping**: Since special schemes guarantee the presence of scheme and host, most fields are non-optional — reducing `if let Some(...)` boilerplate
3. **`wasi:http` integration**: Provide direct conversion to/from the three-part representation used by `wasi:http` Request
4. **RFC 3986 relative resolution**: Support resolving relative references against a base URL (essential for HTTP redirects and HTML link resolution)
5. **Pure computation**: No I/O, no effects — usable in any world

### Scope

#### In Scope

- Special schemes: `http`, `https`, `ws`, `wss`
- WHATWG-style parsing with default port normalization
- Relative URL resolution (RFC 3986 §5)
- URL normalization (case, dot-segments, default ports, percent-encoding)
- Percent-encoding / decoding utilities
- Query string parsing (`application/x-www-form-urlencoded`)
- `wasi:http` interop helpers
- `Eq`, `Display` trait implementations

#### Out of Scope

- Non-special schemes (`mailto:`, `urn:`, `data:`, `file:`, etc.) — can be added as `core:uri` later if needed
- IRI / IDNA / Punycode (internationalized domain names)
- WHATWG error recovery (tab/newline stripping, backslash normalization) — Wado is a server/CLI language, not a browser
- `URLSearchParams` live synchronization — `TreeMap<String, String>` via `parse_query` / `format_query` is sufficient

### Prior Art

| Language                    | Type                | Special Schemes | Relative Resolution      | Query Params                 |
| --------------------------- | ------------------- | --------------- | ------------------------ | ---------------------------- |
| **JavaScript** (WHATWG)     | `URL` class         | Yes (8 schemes) | Via `new URL(ref, base)` | `URLSearchParams` (live)     |
| **Rust** (`url` crate)      | `Url` struct        | Yes (WHATWG)    | `join()` method          | Manual or `form_urlencoded`  |
| **Go** (`net/url`)          | `URL` struct        | No (RFC 3986)   | `ResolveReference()`     | `Values` map                 |
| **Python** (`urllib.parse`) | `ParseResult` tuple | No (RFC 3986)   | `urljoin()`              | `parse_qs()` / `urlencode()` |

Wado's design is closest to Rust's `url` crate in capability, but uses pub fields and value semantics instead of accessor methods, and limits scope to special schemes for simplicity.

## Decision

### Module: `core:url`

```wado
use { Url, ParseError, percent_encode, percent_decode, parse_query, format_query } from "core:url";
```

### `Url` Struct

```wado
/// A parsed URL with a special scheme (http, https, ws, wss).
///
/// Fields store raw (percent-encoded) values. Use `percent_decode` to get
/// decoded forms where needed.
pub struct Url {
    /// The scheme, lowercase. One of: "http", "https", "ws", "wss".
    pub scheme: String,

    /// The username component. Empty string if absent.
    pub username: String,

    /// The password component. Empty string if absent.
    pub password: String,

    /// The host. Always present for special schemes.
    /// IPv6 addresses are stored without brackets (e.g., "::1" not "[::1]").
    pub host: String,

    /// The port number. None means the default port for the scheme is used,
    /// or no port was specified (equivalent for special schemes).
    pub port: Option<u16>,

    /// The path component. Always starts with "/". At minimum "/".
    pub path: String,

    /// The query string without the leading "?". None if absent.
    pub query: Option<String>,

    /// The fragment without the leading "#". None if absent.
    pub fragment: Option<String>,
}
```

### Default Ports

| Scheme  | Default Port |
| ------- | ------------ |
| `http`  | 80           |
| `https` | 443          |
| `ws`    | 80           |
| `wss`   | 443          |

When parsing, if the port matches the default for the scheme, `port` is set to `None`. This follows WHATWG behavior: `http://example.com:80/` and `http://example.com/` produce the same `Url`.

### `ParseError` Variant

```wado
pub variant ParseError {
    EmptyInput,
    InvalidScheme,
    MissingHost,
    InvalidHost,
    InvalidPort,
    InvalidPath,
    InvalidPercentEncoding,
}
```

### `Url` Methods

```wado
impl Url {
    /// Parses a URL string. Only special schemes (http, https, ws, wss)
    /// are accepted.
    pub fn parse(input: String) -> Result<Url, ParseError>

    /// Serializes the URL back to a string.
    pub fn to_string(&self) -> String

    /// Returns the authority: [userinfo@]host[:port].
    /// Suitable for wasi:http Request.set_authority().
    pub fn authority(&self) -> String

    /// Returns the origin: scheme://host[:port] (excludes credentials).
    pub fn origin(&self) -> String

    /// Returns path with query: /path[?query].
    /// Suitable for wasi:http Request.set_path_with_query().
    pub fn path_with_query(&self) -> String

    /// Returns the effective port (explicit port or default for the scheme).
    pub fn effective_port(&self) -> u16

    /// Resolves a relative URL reference against this URL as the base.
    /// Follows RFC 3986 §5 resolution algorithm.
    pub fn resolve(&self, reference: String) -> Result<Url, ParseError>

    /// Parses the query string into a TreeMap (insertion-order preserved).
    /// Decodes percent-encoding and treats "+" as space.
    /// Duplicate keys use last-value-wins semantics.
    /// Returns an empty TreeMap if query is None.
    pub fn query_pairs(&self) -> TreeMap<String, String>

    /// Returns the value for the given query parameter key.
    pub fn query_get(&self, key: String) -> Option<String>

    /// Returns all values for the given query parameter key.
    /// Computed from the raw query string to preserve duplicate keys.
    pub fn query_get_all(&self, key: String) -> List<String>

    /// Returns a new Url with the query string replaced by the given params.
    /// Keys and values are percent-encoded.
    pub fn with_query_pairs(&self, params: TreeMap<String, String>) -> Url
}
```

### `wasi:http` Integration

`core:url` does not depend on `wasi:http`. Integration is done through an infallible constructor that accepts the parts produced by `wasi:http` `Request`:

```wado
impl Url {
    /// Constructs a Url from its scheme, authority, path-with-query, and
    /// fragment. Infallible: any scheme is accepted, malformed authorities
    /// are stored as-is in `host`, and missing or empty paths default to "/".
    pub fn from_parts(
        scheme: String,
        authority: String,
        path_with_query: Option<String>,
        fragment: Option<String>,
    ) -> Url

    /// Returns the query parameters as a TreeMap (percent-decoded).
    pub fn query_params(&self) -> TreeMap<String, String>
}
```

Usage with `wasi:http`:

```wado
use { Url } from "core:url";
use { Request, Scheme } from "wasi:http";

// Construct URL for outgoing request
if let Ok(url) = Url::parse("https://api.example.com/v1/users?page=2") {
    let req = Request::new();
    let scheme = if url.scheme == "https" { Scheme::Https } else { Scheme::Http };
    req.set_scheme(Option::Some(scheme));
    req.set_authority(Option::Some(url.authority()));
    req.set_path_with_query(Option::Some(url.path_with_query()));
}

// Parse URL from incoming request
export async fn handle(request: Request) -> Result<Response, ErrorCode> {
    let scheme = match request.get_scheme() {
        Some(Http) => "http",
        Some(Https) => "https",
        Some(Other(s)) => s,
        None => "http",
    };
    let authority = if let Some(a) = request.get_authority() { a } else { "" };
    let url = Url::from_parts(scheme, authority, request.get_path_with_query(), null);
    // url.path, url.query_params(), url.to_string(), ...
}
```

### Trait Implementations

URLs are normalized at parse time (scheme/host lowercased, dot-segments resolved, default ports removed, unnecessary percent-encoding decoded). This means `Eq` can use direct field comparison without allocating:

```wado
impl Eq for Url {
    fn eq(&self, other: &Self) -> bool {
        return self.scheme == other.scheme
            && self.username == other.username
            && self.password == other.password
            && self.host == other.host
            && self.port == other.port
            && self.path == other.path
            && self.query == other.query
            && self.fragment == other.fragment;
    }
}

/// Display outputs the serialized URL string.
impl Display for Url {
    fn fmt(&self, f: &mut Formatter) {
        f.write_str(&self.to_string());
    }
}

/// Serializes as a JSON string.
impl Serialize for Url { ... }

/// Deserializes from a JSON string via Url::parse.
impl Deserialize for Url { ... }
```

### Module-Level Utility Functions

```wado
/// Percent-encodes a string. Encodes all characters except unreserved
/// characters (A-Z, a-z, 0-9, '-', '.', '_', '~') per RFC 3986 §2.3.
/// Use this for encoding individual path segments or query parameter
/// keys/values.
pub fn percent_encode(input: String) -> String

/// Decodes percent-encoded sequences (%XX).
/// Returns Err if the input contains invalid percent-encoding
/// (e.g., "%GG") or produces invalid UTF-8.
pub fn percent_decode(input: String) -> Result<String, ParseError>

/// Parses a query string into a TreeMap (insertion-order preserved).
/// Input should not include the leading "?".
/// Handles "+" as space and decodes percent-encoding.
/// Duplicate keys use last-value-wins semantics.
pub fn parse_query(query: String) -> TreeMap<String, String>

/// Formats a TreeMap into a query string (insertion-order preserved).
/// Output does not include the leading "?".
/// Keys and values are percent-encoded; spaces become "+".
pub fn format_query(params: TreeMap<String, String>) -> String
```

### Complete API Summary

| Function / Method           | Signature                                               | Description               |
| --------------------------- | ------------------------------------------------------- | ------------------------- |
| `Url::parse`                | `String -> Result<Url, ParseError>`                     | Parse URL string          |
| `Url::from_parts`           | `String, String, Option<String>, Option<String> -> Url` | Infallible parts ctor     |
| `.query_params()`           | `&self -> TreeMap<String, String>`                      | Parse `query` field       |
| `.to_string()`              | `&self -> String`                                       | Serialize to string       |
| `.authority()`              | `&self -> String`                                       | `[user:pass@]host[:port]` |
| `.origin()`                 | `&self -> String`                                       | `scheme://host[:port]`    |
| `.path_with_query()`        | `&self -> String`                                       | `/path[?query]`           |
| `.effective_port()`         | `&self -> u16`                                          | Port or scheme default    |
| `.resolve(ref)`             | `&self, String -> Result<Url, ParseError>`              | Relative resolution       |
| `.query_pairs()`            | `&self -> TreeMap<String, String>`                      | Parse query params        |
| `.query_get(key)`           | `&self, String -> Option<String>`                       | Get param value           |
| `.query_get_all(key)`       | `&self, String -> List<String>`                         | Get all values for key    |
| `.with_query_pairs(params)` | `&self, TreeMap<String, String> -> Url`                 | Replace query params      |
| `percent_encode`            | `String -> String`                                      | Encode for URI component  |
| `percent_decode`            | `String -> Result<String, ParseError>`                  | Decode %XX sequences      |
| `parse_query`               | `String -> TreeMap<String, String>`                     | Parse query string        |
| `format_query`              | `TreeMap<String, String> -> String`                     | Format query string       |

8 pub fields. 13 methods. 4 module functions. 1 error variant. Serde: `Serialize` + `Deserialize` via JSON string.

### Usage Examples

```wado
use { Url, ParseError, percent_encode, parse_query, format_query } from "core:url";

export fn run() with Stdout {
    // Parse a full URL
    if let Ok(url) = Url::parse("https://user:pass@example.com:8443/api/v1?key=val&lang=ja#sec1") {
        assert url.scheme == "https";
        assert url.username == "user";
        assert url.password == "pass";
        assert url.host == "example.com";
        assert url.port matches { Some(8443) };
        assert url.path == "/api/v1";
        assert url.query matches { Some("key=val&lang=ja") };
        assert url.fragment matches { Some("sec1") };

        // Derived accessors
        assert url.authority() == "user:pass@example.com:8443";
        assert url.origin() == "https://example.com:8443";
        assert url.path_with_query() == "/api/v1?key=val&lang=ja";
        assert url.effective_port() == 8443;

        // Query parameters
        assert url.query_get("key") matches { Some("val") };
        assert url.query_get("lang") matches { Some("ja") };
        assert url.query_get("missing") matches { None };

        // Resolve relative URL
        if let Ok(resolved) = url.resolve("../v2/items?page=1") {
            assert resolved.path == "/api/v2/items";
            assert resolved.query matches { Some("page=1") };
            assert resolved.host == "example.com";
        }
    }

    // Default port normalization
    if let Ok(url) = Url::parse("http://example.com:80/path") {
        assert url.port matches { None };  // 80 is default for http
        assert url.effective_port() == 80;
        assert url.to_string() == "http://example.com/path";
    }

    // Construct from fields
    let url = Url {
        scheme: "https",
        username: "",
        password: "",
        host: "example.com",
        port: null,
        path: "/search",
        query: Option::Some(`q={percent_encode("hello world")}`),
        fragment: null,
    };
    assert url.to_string() == "https://example.com/search?q=hello%20world";

    // Query string utilities (TreeMap preserves insertion order)
    let params = parse_query("name=Alice&age=30");
    assert params.get("name") matches { Some("Alice") };
    assert params.get("age") matches { Some("30") };

    let mut qp = TreeMap::<String, String>::new();
    qp["key"] = "hello world";
    qp["lang"] = "日本語";
    let qs = format_query(qp);
    // "key=hello+world&lang=%E6%97%A5%E6%9C%AC%E8%AA%9E"

    // Error handling
    assert Url::parse("") matches { Err(EmptyInput) };
    assert Url::parse("mailto:user@host") matches { Err(InvalidScheme) };
    assert Url::parse("http://") matches { Err(MissingHost) };
}
```

### Implementation Notes

The implementation will be pure Wado, following the same pattern as `core:base64` and `core:json`. No new compiler features are required.

| File                                   | Description    |
| -------------------------------------- | -------------- |
| `wado-compiler/lib/core/url.wado`      | Implementation |
| `wado-compiler/lib/core/url_test.wado` | Tests          |

Key implementation details:

- **Parsing**: Hand-written recursive descent parser. No regex dependency.
- **Percent-encoding tables**: Module-level `global` arrays for encode/decode lookup.
- **IPv6 host parsing**: Validate bracket syntax on input, store address without brackets internally.
- **Dot-segment removal**: Implement RFC 3986 §5.2.4 `remove_dot_segments` algorithm.
- **Query string encoding**: `application/x-www-form-urlencoded` — space as `+`, percent-encode the rest.

## Consequences

### Positive

- **Option-light API**: Only 3 of 8 fields are optional, compared to 6 of 7 in a RFC 3986 URI type — less unwrapping at every use site
- **`wasi:http` ready**: `authority()`, `path_with_query()`, and `from_parts()` map directly to the wasi:http Request API
- **Practical scope**: Special schemes cover the vast majority of Wado use cases (HTTP clients, servers, WebSocket endpoints)
- **No effects**: Pure functions, usable in any world
- **Familiar model**: Developers who know JavaScript's `URL` or Rust's `url` crate will find the API unsurprising

### Negative

- **No opaque URIs**: `mailto:`, `urn:`, `data:` URLs cannot be represented. A separate `core:uri` module could be added later for RFC 3986 URI-references
- **No IDNA**: Internationalized domain names are not normalized to Punycode. The host is stored as-is. This could be added as a separate utility without changing the `Url` type
- **Eq field comparison**: Comparing two URLs compares all fields individually, which is slightly more expensive than a single string comparison. For hot-path comparisons, users can compare `to_string()` results

### Future Extensions

- `core:uri` — RFC 3986 URI-reference type for non-special schemes
- IDNA / Punycode support as a separate utility module
- `impl Hash for Url` for use in hash-based collections

## Related WEPs

- [WASI HTTP Integration](./wep-2026-02-21-wasi-http.md) — the primary consumer of `core:url`
- [String Type Design](./wep-2026-01-15-string-type-design.md) — string manipulation used in URL parsing
- [Redesign String and List APIs](./wep-2026-03-29-redesign-string-array-api.md) — string methods used in implementation
- [Base64 Encoding API](./wep-2026-02-27-base64-api.md) — similar stdlib module design pattern

## References

- [WHATWG URL Living Standard](https://url.spec.whatwg.org/)
- [RFC 3986 — Uniform Resource Identifier (URI): Generic Syntax](https://datatracker.ietf.org/doc/html/rfc3986)
- [RFC 3987 — Internationalized Resource Identifiers (IRIs)](https://datatracker.ietf.org/doc/html/rfc3987) — out of scope, noted for future
