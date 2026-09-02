# WEP: JSON Web Tokens (`core:jwt`)

## Context

A program that serves `wasi:http/service` needs to check a bearer token, and
one that issues sessions needs to mint them. JWT (RFC 7519) over JWS Compact
Serialization (RFC 7515) is what that traffic actually carries.

The pieces were already in the tree: `core:digest` hashes with SHA-256,
`core:base64` encodes URL-safe base64, `core:json` and `core:serde`
deserialize claims. What was missing is HMAC, the token grammar, and the
rejection rules.

Two questions decide the shape of the module, and neither is about the math:

- **Which algorithms.** RFC 7518 requires only `HS256` of an implementation
  (`RS256` is Recommended, `ES256` Recommended+). A self-issued session token —
  the same program signs and verifies — is served entirely by `HS256`. An
  OIDC ID token from an external identity provider never is: those are `RS256`
  or `ES256`, because a relying party cannot hold the issuer's secret.
  `RS256` needs bigint modular exponentiation, PKCS#1 v1.5, and a JWK/SPKI
  parser; `ES256` needs P-256 field arithmetic. Both are practical over the
  wide-multiply builtin (`builtin::i64_mul_wide_u`) at verification speeds an
  HTTP service can pay per request — the cost is the implementation and its
  security review, not the runtime.
- **How much of the token layer.** Decoding claims through `core:json` costs
  several times what verifying a signature does, in code size. What the module
  forces on every caller therefore matters more than what it offers.

## Decision

Ship the minimum that is correct on its own terms, with the seam that lets the
rest arrive from outside it.

### The key decides the algorithm

`verify(token, &key)` checks the signature with the key's algorithm. The
header cannot select a verifier, so the algorithm-confusion class (`alg: none`,
an RSA public key replayed as an HMAC secret) has no entry point. `alg` is
still read and must match the key's, and a `crit` header is rejected outright
(RFC 7515 §4.1.11), because this module implements no extension.

### Algorithms are a trait, not an enum

```wado
pub trait JwsAlgorithm { fn alg(&self) -> String; }
pub trait JwsVerifier: JwsAlgorithm { fn verify(&self, signing_input: &ByteList, signature: &ByteList) -> bool; }
pub trait JwsSigner: JwsAlgorithm { fn sign(&self, signing_input: &ByteList) -> ByteList; }
```

A closed `variant Alg { HS256, RS256, … }` would make every future algorithm a
change to this module. A trait lets a package define an `Rs256Key` that plugs
into the same `verify`, with `core:jwt` unchanged — the minimum stays a
minimum without becoming a dead end. `Hs256Key` is the one implementation
here, signing and verifying with HMAC-SHA256 (RFC 2104).

### Claims stay bytes

`verify` returns the payload bytes. A caller that wants typed claims
deserializes them itself — into its own struct, or into `RegisteredClaims`
(`iss`, `sub`, `exp`, `nbf`, `iat`). Because a program links only what it
calls, a caller that verifies a signature and reads the payload its own way
never pulls `core:json` into its component.

### Time is an argument

`RegisteredClaims::validate_time(now, leeway)` takes the current time rather
than reading a clock, so verifying a token adds nothing to a program's effect
row. A caller that has `SystemClock` passes `Instant::now().seconds`.

### Canonical base64url lives in `core:base64`

JWS mandates URL-safe base64 without padding. `core:base64::decode` is
deliberately lenient — it accepts either alphabet, with or without padding,
and ignores a final group's unused bits — so one signature has several
spellings that all verify. Tokens are used as cache and revocation keys, where
that is a bug.

`decode_url` is the canonical inverse of `encode_url`: it rejects `+`, `/`,
`=`, and a final group whose dropped bits are set, so one byte string has
exactly one accepted spelling. It belongs in `core:base64` next to the encoder
it inverts, not hidden inside `core:jwt`.

### Refusing a weak secret

`Hs256Key::new` asserts a secret of at least 32 bytes, as RFC 7518 §3.2
requires. A brute-forcible HMAC secret is the most common JWT deployment
failure, and an assert fires at key construction — startup — not per request.

## Consequences

- A program that verifies self-issued tokens carries little beyond SHA-256 and
  base64, and verifies a token in well under the cost of the request that
  carries it.
- `RS256` / `ES256` can be added as a package, or promoted into `core:jwt`
  later, without a breaking change to callers.
- The module carries no clock, no network, and no key-set logic, so it neither
  fetches JWKS nor rotates keys. That is the caller's, and it needs no
  cooperation from here.
- Signature comparison is constant-time (`ct_eq`), and it is exported so an
  out-of-tree verifier compares the same way. Neither the optimizer nor Wasm
  guarantees constant-time execution generally, and the module does not claim
  side-channel resistance beyond that comparison.

## Known gaps

- **`aud`** is a string _or_ an array of strings on the wire.
  `RegisteredClaims` omits it rather than picking one; closing this takes an
  untagged-union deserializer for that field.
- **`HS384` / `HS512`** need SHA-384/512 in `core:digest`, the 64-bit word
  variants of the existing SHA-256.
- **`RS256` / `ES256`**: bigint with Montgomery multiplication over the
  wide-multiply builtin, PKCS#1 v1.5 or PSS, and JWK/SPKI parsing; P-256 field
  and group arithmetic on top of the same bigint.
- **A mixed key set.** Wado has no dynamic dispatch, so selecting among several
  algorithms at runtime — a JWKS carrying both RSA and EC keys — needs a
  closed `match` at the call site. Generic `verify` covers the static case
  only.
- **`hmac_sha256` is private to `core:jwt`.** HMAC over the `Digest` trait
  belongs in `core:digest`; generalizing it needs a block size per algorithm,
  which traits cannot express today (no associated constants in a trait).
- **JWE** (encrypted tokens, RFC 7516) is a separate specification and is not
  addressed here.
