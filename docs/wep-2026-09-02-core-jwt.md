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

Ship the algorithm every JWT program needs, and shape the module so the rest
arrives without rewriting it. Four rules do that work.

### The key decides the algorithm

`verify(token, &key)` checks the signature with the key's algorithm. The
header cannot select a verifier, so the algorithm-confusion class has no entry
point: neither `alg: none` nor an RSA public key replayed as an HMAC secret.
`alg` is still read and must match the key's. A `crit` header is rejected
outright (RFC 7515 §4.1.11), because this module implements no extension.

This is what makes the seam safe to open. A new algorithm arrives as a new key
type, and a key type can only ever verify with itself.

### Algorithms are a trait, not an enum

```wado
pub trait JwsAlgorithm { fn alg(&self) -> String; }
pub trait JwsVerifier: JwsAlgorithm { fn verify(&self, signing_input: &ByteList, signature: &ByteList) -> bool; }
pub trait JwsSigner: JwsAlgorithm { fn sign(&self, signing_input: &ByteList) -> ByteList; }
```

A closed `variant Alg { HS256, RS256, … }` would make every future algorithm a
change to this module, and a package could add none. A trait lets a package
define an `Rs256Key` that plugs into the same `verify`, with `core:jwt`
unchanged. `Hs256Key` is the one implementation here, signing and verifying
with HMAC-SHA256 (RFC 2104).

The traits say nothing about keys, curves, or padding. They take the signing
input and the signature as bytes, which is all JWS itself defines.

### One entry point, and claims stay bytes

`verify` is the only way in, and it returns the payload bytes. A second
function that verified the signature without reading the header would be a
door around the `alg` and `crit` checks, and the caller who took it would owe
itself both.

A caller that wants typed claims deserializes the payload itself, into its own
struct or into `RegisteredClaims` (`iss`, `sub`, `exp`, `nbf`, `iat`) for the
registered ones. The module reads no claim of its own.

### Primitives belong to the layer below

Nothing general lives here. Keyed hashing is `core:digest`'s `hmac`, over the
`Digest` trait, with `hmac_sha256` as the one-shot counterpart to `sha256`.
Canonical base64url is `core:base64`'s `decode_url_strict`. Constant-time
comparison is `core:prelude`'s `eq_constant_time`, which every verifier
reaches without an import.

Each of those has callers beyond tokens, and each is what a future algorithm
will build on. A `core:jwt` that hoarded them would make an RSA package
import a token library to get a MAC.

## What it does today

- `HS256` end to end: `sign` writes `{"alg":"HS256","typ":"JWT"}` and the
  compact serialization; `verify` checks three canonical base64url segments,
  the signature, `alg`, and `crit`.
- `Hs256Key::new` asserts a secret of at least 32 bytes, as RFC 7518 §3.2
  requires. A brute-forcible HMAC secret is the most common JWT deployment
  failure, and the assert fires at key construction, which is startup rather
  than per request.
- `RegisteredClaims::validate_time(now, leeway)` checks `exp` and `nbf`
  against a time it is passed rather than a clock it reads, so verifying a
  token adds nothing to a program's effect row. A caller that has
  `SystemClock` passes `Instant::now().seconds`.
- A key type defined outside this module verifies through the same `verify`,
  which the tests exercise with a stub `RS256` key.

## What extension opens

Each of these attaches to the seam already in place, and none of them changes
the entry point.

- `HS384` and `HS512` need SHA-384/512 in `core:digest`, the 64-bit word
  variants of the existing SHA-256. `hmac` already keys any `Digest`, so the
  key types are then a few lines each.
- `RS256` and `ES256` need bigint with Montgomery multiplication over the
  wide-multiply builtin, PKCS#1 v1.5 or PSS, and JWK or SPKI parsing. P-256
  field and group arithmetic sits on top of the same bigint. Either can live
  in a package first and be promoted here later, and callers see no
  difference: both spell `verify(token, &key)`.
- A JWKS-shaped API, where a `kid` in the header selects a key before
  verification, needs a way to read an unverified header. That is a real need,
  and a sharp edge. It should arrive with the algorithms that motivate it,
  named for the unverified bytes it hands back rather than offered as a second
  `verify`.
- `aud` is a string _or_ an array of strings on the wire. `RegisteredClaims`
  omits it rather than picking one, and closing this takes an untagged-union
  deserializer for that field.

## Security

### The threat model

The attacker writes the whole token, header and payload and signature alike,
and sends as many as it likes. It does not hold the key. Under that, `verify`
promises one thing: it returns a payload only for a token whose signature is
this key's over these exact bytes.

### What the design refuses

Each of these is a decision above, and each has a test that fails if the
decision is undone.

- Algorithm confusion, including `alg: none` and an RSA public key replayed as
  an HMAC secret. The key selects the verifier, so a header cannot.
- A signature the key did not write, including a stripped one. A token is
  three segments, and the third is checked before the first two are parsed.
- A second spelling of the same token. Canonical base64url gives one byte
  string one encoding, which matters wherever a token is a cache key, a
  revocation-list key or a deduplication key.
- A timing oracle on the MAC. The comparison does not stop at the first
  differing byte, so its duration says nothing about how much of a forgery was
  right.
- A brute-forcible secret, refused at construction rather than per request.
- A `crit` header naming extensions this module does not implement.

The order of checks is part of this. `verify` reaches the JSON parser only
after the signature holds, so an attacker cannot feed the parser without the
key.

### What it does not cover

- Key management. Storage, rotation, revocation and JWKS retrieval are the
  caller's, and this module holds no opinion about them.
- Replay. A valid token stays valid until `exp`; there is no `jti` ledger and
  no nonce.
- Claims semantics. `iss`, `aud` and scopes mean what the application says
  they mean. `exp` is checked only when the caller calls `validate_time`; the
  module never checks it on its own.
- Resource bounds. A token is base64-decoded before its MAC is checked, so an
  oversized request should be refused before it reaches `verify`.
- Side channels beyond that one comparison. Neither the optimizer nor Wasm
  guarantees constant-time execution.

### What a new algorithm owes

- Never consult the header. `verify` receives the signing input and the
  signature, and that is all it may use.
- Compare with `eq_constant_time`.
- Reject on any parse failure. A malformed key or signature is a rejection,
  never a fallback to a default.
- Treat private-key operations as out of the module's reach: their side
  channels are the implementation's problem. ECDSA in particular needs a
  deterministic nonce (RFC 6979), since a repeated nonce discloses the key and
  randomness here is an effect a signer may not have.

### How the rules are checked

The tests carry RFC 7515 A.1 for the token and RFC 4231 for the MAC, and the
negative half is the point: a tampered payload, another key, a padded
signature, a header whose `alg` disagrees, a `crit` header. Public-key
algorithms should arrive with Wycheproof vectors, which cover the malformed
inputs an RFC's examples do not.

## Consequences

- A program that verifies self-issued tokens carries little beyond SHA-256 and
  base64, and verifies a token in well under the cost of the request that
  carries it.
- The module carries no clock, no network, and no key-set logic, so it neither
  fetches JWKS nor rotates keys. That work is the caller's, and it needs no
  cooperation from here.

## Known gaps

- A mixed key set has no answer yet. Wado has no dynamic dispatch, so choosing
  among algorithms at runtime, as a JWKS carrying both RSA and EC keys asks,
  needs a closed `match` at the call site. Generic `verify` covers the static
  case only, and this is the one place where the trait seam stops short.
- JWE (encrypted tokens, RFC 7516) is a separate specification and is not
  addressed here.
