# WEP: URI References as Identifiers (`core:uri`)

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

`core:uri` implements RFC 3986 for URI references used as identifiers. It reads
one, resolves one against a base, and writes one back. Every scheme is read the
same way, by the generic syntax.

```text
pub struct Uri { … }   // components private, valid by construction

impl FromStr for Uri;              // URI-reference (§4.1), or a UriError
impl Uri {
    pub fn resolve(&self, reference: &Uri) -> Uri;   // §5.2; self needs a scheme
    pub fn fragment(&self) -> Option<StrSlice>;
    pub fn without_fragment(&self) -> Uri;
}
impl Display for Uri;              // recomposition (§5.3)
```

`Uri` derives `Eq` and `Hash`, so it keys a `HashMap`. Two values are equal
when their components are equal as written.

### What a `Uri` holds

A `Uri` holds the five components of §3: scheme, authority, path, query and
fragment. Scheme, authority, query and fragment may be absent, which is
distinct from present and empty: `a?` has an empty query, and `a` has none.
The authority is one component, not a host and a port, since an identifier
compares it as a whole.

A component outside the grammar of Appendix A is a `UriError` at its byte
offset, so every `Uri` is one RFC 3986 admits. Only ASCII is admitted: an IRI
(RFC 3987) is not a URI.

### Resolution

`resolve` is the algorithm of §5.2.2 with the dot-segment removal of §5.2.4.
The base must have a scheme, as §5.1 requires; a call with one that does not
breaks the function's contract.

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
