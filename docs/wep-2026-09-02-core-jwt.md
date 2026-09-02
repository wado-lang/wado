# WEP: JSON Web Tokens (`core:jwt`)

## Context

A program that serves `wasi:http/service` needs to check a bearer token, and
one that issues sessions needs to mint them. JWT (RFC 7519) over JWS Compact
Serialization (RFC 7515) is what that traffic actually carries.

The pieces were already in the tree: `core:digest` hashes with SHA-256,
`core:base64` encodes URL-safe base64, `core:json` and `core:serde`
deserialize claims. What was missing is keyed hashing, the token grammar, and
the rejection rules.

Two questions decide the shape of the module, and neither is about the math.

### Which algorithms

RFC 7518 requires only `HS256` of an implementation; `RS256` is Recommended
and `ES256` Recommended+. `HS256` serves a self-issued session token, where
the same program signs and verifies. It cannot serve an OIDC ID token from an
external identity provider: those are signed `RS256` or `ES256`, because a
relying party cannot hold the issuer's secret.

`RS256` needs bigint modular exponentiation, PKCS#1 v1.5, and a JWK/SPKI
parser. `ES256` needs P-256 field arithmetic. Both verify fast enough over the
wide-multiply builtin (`builtin::i64_mul_wide_u`) for an HTTP service to pay
per request. What they cost is the implementation and its security review.

### How much of the token layer

Claims are an application schema, not a token format: every deployment reads
different ones. A JWT library that owns the claim structs owns a schema it
cannot know.

## Decision

Ship the minimum that is correct on its own terms, with the seam that lets the
rest arrive from outside it.

### The key decides the algorithm

`verify(token, &key)` checks the signature with the key's algorithm. The
header cannot select a verifier, so the algorithm-confusion class has no entry
point: neither `alg: none` nor an RSA public key replayed as an HMAC secret.
`alg` is still read and must match the key's. A `crit` header is rejected
outright (RFC 7515 §4.1.11), because this module implements no extension.

### Algorithms are a trait, not an enum

```wado
pub trait JwsAlgorithm { fn alg(&self) -> String; }
pub trait JwsVerifier: JwsAlgorithm { fn verify(&self, signing_input: &ByteList, signature: &ByteList) -> bool; }
pub trait JwsSigner: JwsAlgorithm { fn sign(&self, signing_input: &ByteList) -> ByteList; }
```

A closed `variant Alg { HS256, RS256, … }` would make every future algorithm a
change to this module. A trait lets a package define an `Rs256Key` that plugs
into the same `verify`, with `core:jwt` unchanged, so the minimum stays a
minimum without becoming a dead end. `Hs256Key` is the one implementation
here, signing and verifying with HMAC-SHA256 (RFC 2104).

### One entry point, and claims stay bytes

`verify` is the only way in, and it returns the payload bytes. A second
function that verified the signature without reading the header would be a
door around the `alg` and `crit` checks, and the caller who took it would owe
itself both. Reading a header _before_ verification is a real need — a `kid`
that selects a key — but that shape is JWKS's, and it can arrive as its own
API with the algorithms that need it.

A caller that wants typed claims deserializes the payload itself, into its own
struct or into `RegisteredClaims` (`iss`, `sub`, `exp`, `nbf`, `iat`) for the
registered ones. The module reads no claim of its own.

### HMAC belongs to `core:digest`

Keyed hashing is not a JWT concept. `core:digest` gains `hmac` over the
`Digest` trait, plus `hmac_sha256` as the one-shot counterpart to `sha256`.
The trait gains `block_len` as a method, since a trait cannot declare an
associated constant today. `core:jwt` is then a consumer, and the next caller
that needs a MAC does not go looking inside a token library.

### Time is an argument

`RegisteredClaims::validate_time(now, leeway)` takes the current time rather
than reading a clock, so verifying a token adds nothing to a program's effect
row. A caller that has `SystemClock` passes `Instant::now().seconds`.

### Canonical base64url lives in `core:base64`

JWS mandates URL-safe base64 without padding. `core:base64::decode` is
deliberately lenient: it accepts either alphabet, with or without padding, and
ignores a final group's unused bits. One signature therefore has several
spellings that all verify, and tokens are used as cache and revocation keys,
where that is a bug.

`decode_url_strict` is the canonical inverse of `encode_url`. It rejects `+`,
`/`, `=`, and a final group whose dropped bits are set, so one byte string has
exactly one accepted spelling. It belongs in `core:base64` next to the encoder
it inverts, not hidden inside `core:jwt`. Its name answers "why this one, when
`decode` already takes base64url?" without a trip to the docs.

### A weak secret is refused

`Hs256Key::new` asserts a secret of at least 32 bytes, as RFC 7518 §3.2
requires. A brute-forcible HMAC secret is the most common JWT deployment
failure, and the assert fires at key construction, which is startup rather
than per request.

### Comparing a MAC

`Hs256Key` compares through `core:prelude`'s `eq_constant_time`, and an
out-of-tree verifier reaches the same function without importing anything.
Constant-time comparison is where a MAC leaks if anywhere, and a comparison
that stops at the first differing byte says how much of a forgery was right.

## Consequences

- A program that verifies self-issued tokens carries little beyond SHA-256 and
  base64, and verifies a token in well under the cost of the request that
  carries it.
- `RS256` and `ES256` can be added as a package, or promoted into `core:jwt`
  later, without a breaking change to callers.
- The module carries no clock, no network, and no key-set logic, so it neither
  fetches JWKS nor rotates keys. That work is the caller's, and it needs no
  cooperation from here.
- Neither the optimizer nor Wasm guarantees constant-time execution, so the
  module claims no side-channel resistance beyond the comparison it makes.

## Known gaps

- `aud` is a string _or_ an array of strings on the wire. `RegisteredClaims`
  omits it rather than picking one, and closing this takes an untagged-union
  deserializer for that field.
- `HS384` and `HS512` need SHA-384/512 in `core:digest`, the 64-bit word
  variants of the existing SHA-256.
- `RS256` and `ES256` need bigint with Montgomery multiplication over the
  wide-multiply builtin, PKCS#1 v1.5 or PSS, and JWK/SPKI parsing. P-256 field
  and group arithmetic sits on top of the same bigint.
- A mixed key set has no answer yet. Wado has no dynamic dispatch, so choosing
  among algorithms at runtime, as a JWKS carrying both RSA and EC keys asks,
  needs a closed `match` at the call site. Generic `verify` covers the static
  case only.
- JWE (encrypted tokens, RFC 7516) is a separate specification and is not
  addressed here.
