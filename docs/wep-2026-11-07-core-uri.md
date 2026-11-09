# WEP: URIs and URI References as Identifiers (`core:uri`)

## Context

`core:url` follows the WHATWG URL Standard for the special schemes (`http`,
`https`, `ws`, `wss`), and
[its WEP](./wep-2026-04-10-url-stdlib.md) leaves the other schemes to a later
`core:uri`. A URL is something to fetch, so `core:url` parses hosts, ports and
query parameters.

JSON Schema needs the other half. A schema's `$id` and `$ref` are URI references
under RFC 3986, and [Jade](./wep-2026-06-13-jade.md)'s validator resolves each
against the base URI in force. The JSON Schema Test Suite names schemas by
`urn:uuid:…` and `file:///…` as well as by `http://…`. Nothing here is fetched:
a URI only names a schema, and two names are compared.

## Decision

`core:uri` implements RFC 3986 for URIs and URI references used as
identifiers. It reads one, resolves a reference against a URI, and writes one
back. Every scheme is read the same way, by the generic syntax.

The two types follow RFC 3986's two rules. A `UriReference` is a
`URI-reference` (§4.1): a URI, or a relative reference. A `Uri` is a `URI`
(§3), one with a scheme. Only a `Uri` is a base, so the type says what §5.1
requires of one, and a reference resolved against it is a `Uri` again.

```text
pub struct Uri { … }            // components private, valid by construction
pub struct UriReference { … }

impl FromStr for Uri;            // URI (§3), or a UriError
impl FromStr for UriReference;   // URI-reference (§4.1), or a UriError
impl Uri {
    pub fn resolve(&self, reference: &UriReference) -> Uri;   // §5.2
    pub fn fragment(&self) -> Option<StrSlice>;
    pub fn without_fragment(&self) -> Uri;
}
impl Display for Uri;            // recomposition (§5.3), as for UriReference
```

`UriReference` has `fragment` and `without_fragment` too. Each type derives
`Eq` and `Hash`, so it keys a `HashMap`. Two values are equal when their
components are equal as written. Each serializes as its text, which is what
JSON Schema's `uri` and `uri-reference` formats describe.

### What a `UriReference` holds

A `UriReference` holds the five components of §3: scheme, authority, path,
query and fragment. Scheme, authority, query and fragment may be absent, which is
distinct from present and empty: `a?` has an empty query, and `a` has none.
The authority is one component, not a host and a port, since an identifier
compares it as a whole.

A component outside the grammar of Appendix A is a `UriError` at its byte
offset, so every value is one RFC 3986 admits. Only ASCII is admitted: an IRI
(RFC 3987) is not a URI. A `Uri` is a `UriReference` whose scheme is present;
text without one is a `UriError` at offset 0.

### Resolution

`resolve` is the algorithm of §5.2.2 with the dot-segment removal of §5.2.4.

A result with no authority whose path starts `//` would read back with that
path as its authority, which §3.3 forbids. `resolve` writes `/.` before such a
path, so `foo:` resolves `/.//bar` to `foo:/.//bar`.

### What `core:uri` leaves out

`core:uri` has none of `core:url`'s conveniences: no host or port accessors, no
query parameters, no percent-decoding. A program that fetches what a URI names
uses `core:url`.

## Known gaps

### Normalization

Two URIs that RFC 3986 §6 treats as equivalent compare unequal unless they are
written alike. `HTTP://a/%7e` and `http://a/~` name one resource, but only the
scheme's case is folded.

## Related WEPs

- [URL Standard Library (`core:url`)](./wep-2026-04-10-url-stdlib.md) — the
  WHATWG URL for the special schemes
- [Jade — JSON Schema for Wado](./wep-2026-06-13-jade.md) — the first user
