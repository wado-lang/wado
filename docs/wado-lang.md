<p align="center">
  <img src="../wado-512.png" alt="Wado" width="180" />
</p>

# Wado

**Type-safe, high-level WebAssembly.**

Type-safe like Rust, familiar like TypeScript, compiled to clean Wasm. Wasm in plain sight.

[View on GitHub →](https://github.com/wado-lang/wado)

---

## A First Look

```wado
// Hello World in Wado

use { println, Stdout } from "core:cli";

export fn run() with Stdout {
    println("Hello, world!");
}
```

Seven lines, but a lot is on display: ES module–style imports from a namespaced standard library (`core:cli`), an `export` keyword that declares the entrypoint of the `wasi:cli/command` world, and a `with Stdout` clause that makes the side effect part of the function's type.

```sh
wado run example/hello.wado
```

The compiled `.wasm` for this program is **1,773 bytes** — smaller than the same program written in C, Zig, Moonbit, or Rust.

---

## Why Wado?

Existing Wasm-targeting languages bundle their own memory management runtime into every `.wasm` file. The result is bloated binaries even for trivial programs:

| Program            |     Wado | Rust (wasm32-wasip1) | ratio |
| ------------------ | -------: | -------------------: | ----: |
| `hello_world`      |  1,773 B |             40,115 B | 22.6× |
| `pi_approx`        |  8,982 B |             59,496 B |  6.6× |
| `zlib` (gzip)      | 18,935 B |             88,563 B |  4.7× |
| `sqlite_highlight` |   622 KB |             3,481 KB |  5.6× |

Wado takes a different approach. By targeting **Wasm GC**, the garbage collector is provided by the host runtime itself — so it doesn't have to ship inside the module. The output is a thin shell of pure logic.

The timing matters too. With the **Wasm Component Model** and **WASI P3** maturing, Wado is designed for this new era from the ground up — no legacy baggage, no retrofitting.

See [`wasm-size/README.md`](../wasm-size/README.md) for the full size comparison across C, Zig, Moonbit, and Rust.

---

## A Real Example: an HTTP Service

The rest of this page tours [`example/http_bin.wado`](../example/http_bin.wado) — a single-file httpbin clone implementing `/get`, `/post`, `/headers`, `/status/:code`, `/base64/:val`, and more, served via the Component Model `wasi:http/service` world.

Run it:

```sh
cargo run --bin wado -- serve example/http_bin.wado
curl http://localhost:8080/get
```

The full source is at the bottom of this section. Below, eleven snippets that each highlight something Wado does well.

### 1. One-file scripts, with the standard library and WASI in scope

A Wado file can be made directly executable with a shebang — one-file scripts are first-class:

```wado
#!/usr/bin/env wado serve
```

The stdlib is namespaced (`core:*` for the language runtime, `wasi:*` for WASI worlds) and imported the way ES modules are:

```wado
use { Request, Response, Method, Scheme, ErrorCode } from "wasi:http";
use { MonotonicClock } from "wasi:clocks";
use { TreeMap } from "core:collections";
use { decode } from "core:base64";
use { Serialize } from "core:serde";
use { Url } from "core:url";
use json from "core:json";
```

Notice that `wasi:http` and `wasi:clocks` are imported the same way as `core:json` — WASI worlds are simply modules.

### 2. Effects belong in the function type

```wado
export async fn handle(request: Request) -> Result<Response, ErrorCode>
    with MonotonicClock {
```

The `with` clause lists the capabilities this function uses. `MonotonicClock` here is a WASI capability, not a global; the runtime grants it on entry. Pure functions have no `with` clause, and you can tell at a glance.

`async fn` declares a Component Model–level async function, which the host can suspend and resume across the canonical ABI.

### 3. Auto-derived JSON, one line

```wado
struct RequestInfo {
    method: String,
    path: String,
    args: TreeMap<String, String>,
    headers: TreeMap<String, String>,
    origin: String,
    url: String,
    data: Option<String>,
}
impl Serialize for RequestInfo;
```

The single line `impl Serialize for RequestInfo;` derives the serializer. There is no separate macro and no schema file — the struct's field types are the schema.

### 4. Exhaustive pattern matching over variants

```wado
fn scheme_name(s: Option<Scheme>) -> String {
    return match s {
        Some(Http) => "http",
        Some(Https) => "https",
        Some(Other(name)) => name,
        None => "http",
    };
}
```

`Scheme` is a variant (a sum type with payloads) imported straight from `wasi:http`. The compiler verifies that every case is covered, including the nested `Other(name)` payload that carries a custom scheme string.

### 5. Declarative naming for the wire format

```wado
#[serde(rename_all = "kebab-case")]
struct UserAgentResponse {
    user_agent: Option<String>,
}
impl Serialize for UserAgentResponse;
```

The Rust-style attribute renames `user_agent` to `user-agent` on the wire. Idiomatic Wado naming on the inside, idiomatic JSON on the outside.

### 6. `match` as an expression, even on strings

```wado
match path {
    "/"            => { content_type = "text/html; charset=utf-8"; body_str = HOMEPAGE; },
    "/get"         => { /* ... */ },
    "/post"        => { /* ... */ },
    "/headers"     => { /* ... */ },
    "/ip"          => { /* ... */ },
    "/user-agent"  => { /* ... */ },
    _              => { /* dynamic routes: /status/:code, /base64/:val, ... */ },
}
```

`match` is an expression and works on string literals as well as variants. The wildcard arm dispatches the dynamic routes through a separate function.

### 7. Template strings with format specifiers

```wado
trailers.append(
    "server-timing",
    `app;dur={elapsed_ms:0.3f}`.as_bytes() as FieldValue,
);
```

Backtick strings interpolate expressions like TypeScript, but the `:0.3f` format specifier is checked at compile time against the value's `Display` impl. The result is `app;dur=12.345`.

### 8. Tuples are first-class return values

```wado
fn echo_if_method(/* ... */) -> [StatusCode, String, Option<String>] {
    /* ... */
    return [200, build_request_json(/* ... */), null];
}

let [s, b, a] = echo_if_method(request, method, Method::Get, "GET", false, url, path, args);
```

Tuple literal syntax (`[a, b, c]`) is the same in types, expressions, and patterns. There is no third syntax for "multiple return values" — there's just tuples and destructuring.

### 9. `task return`: hand off the response, then keep working

```wado
task return Result::Ok(response);

body_tx.write(body_str.as_bytes());
body_tx.drop();
```

In a Component Model `async fn`, `task return` delivers the result to the caller without ending the function. The handler can then continue streaming the body, write trailers, and emit access logs — all after the response status is already on the wire.

### 10. Compile-time file inclusion

```wado
global HOMEPAGE: String = #include_str("./http-bin-index.html");
```

The HTML for `/` lives in a separate file for editing comfort, but ends up baked into the `.wasm` as a constant. There is no filesystem read at runtime, and no separate asset to ship.

### 11. `if let` with guards

```wado
if let Some(a) = request.get_authority() && !a.is_empty() {
    return a;
}
```

Pattern binding and a boolean guard combine in a single condition. The bound name `a` is in scope for both the guard and the body.

The full source is at [`example/http_bin.wado`](../example/http_bin.wado) (~345 lines).

---

## Design Principles

### Wasm in plain sight

No macros. The code you read is the code that runs. This makes Wado predictable to read, predictable to debug, and predictable to optimize — the WAT output is something you can reason about by looking at the source.

### Readable without context switching

Explicit over implicit. No implicit type conversions, no function overloading, no hidden dependencies. You shouldn't need to jump to other files to understand what a function does.

### Type-safe by design

Strong static typing with no escape hatches like `any`. This prevents the defensive programming patterns (excessive `try-catch`, runtime type checks) that tend to creep into dynamically-typed codebases.

### No exception unwinding

Errors are values: `Result<T, E>` and `Option<T>`. Control flow is local and visible. Stack unwinding doesn't exist as a language feature.

### Minimal binary size

By leveraging Wasm GC instead of bundling a runtime, Wado produces compact `.wasm` files. This is the core motivation of the language.

---

## Designed for Agentic Coding

The Wado compiler is **100% agentic-coded** — every line of `wado-compiler` was written by an AI agent — and the same is true of most of the Wado code in this repository, including the `http-bin` example above. The human handles language design and project management; the agents do the rest.

This isn't just a curiosity; it shaped the language itself. After a year of intensive agentic coding, certain patterns became clear:

- **Agents excel at volume but struggle with ambiguity.** Implicit behaviors get multiplied across a codebase. Explicit, predictable semantics work better — so Wado has no implicit conversions, no overloading, no macros.
- **Agents tend toward defensive programming.** Without type safety, they pepper code with runtime type checks and nested error handling. Strong static types, with no `any` escape hatch, eliminate the need.
- **Exceptions break agent reasoning.** Non-local control flow is hard to predict. `Result<T, E>` keeps every failure path visible at the call site.

The result is a language where common agentic-coding pitfalls are eliminated by design, not by convention.

---

## Performance

Wado holds its own on CPU-bound code:

- **Counting primes to 10M** (integer arithmetic): Wado finishes in 3,769 ms — slightly ahead of `gcc -O3` (3,847 ms) and Node.js (4,390 ms).
- **Mandelbrot, 1024×768 at 256 iterations** (float arithmetic): Wado at 188 ms beats Node.js (172 ms is within 1.10×); `gcc -O3` is at 168 ms.

Other workloads — array-heavy, allocation-heavy, JSON-heavy — are still maturing. See [`benchmark/README.md`](../benchmark/README.md) for the full picture across prime counting, Mandelbrot, sieve, float-to-string, zlib, JSON parsing, and SQL parsing.

Per-commit performance is also tracked publicly:

- [Runtime performance dashboard](https://wado-lang.github.io/wado/benchmarks/runtime/)
- [Wasm binary size dashboard](https://wado-lang.github.io/wado/benchmarks/wasm-size/)

---

## Try It in 60 Seconds

Wado isn't on a package registry yet — for now, build from source.

```sh
git clone https://github.com/wado-lang/wado.git
cd wado
cargo build --release
```

Run the hello-world example:

```sh
cargo run --bin wado -- run example/hello.wado
```

Or serve the httpbin clone from this page:

```sh
cargo run --bin wado -- serve example/http_bin.wado
# in another terminal:
curl http://localhost:8080/get
```

For an editor experience, the repo also includes a VS Code extension under [`wado-vscode/`](../wado-vscode/).

---

## Status & Roadmap

Wado is **experimental**. The core language — syntax, static typing, generics, closures, modules, traits, pattern matching, the effect system — is implemented and functional. It is already usable for its original purpose: embedding small, type-safe Wasm modules into JavaScript projects where binary size matters.

That said, Wado's design points further into the future. Three things in the broader Wasm ecosystem need to land before Wado can be what it's meant to be:

- **Browser support for the Wasm Component Model.** Today, Wado components run on wasmtime. Once browsers ship CM, the same `.wasm` will run unchanged on the web.
- **WASI 1.0.** WASI P3 is the current release-candidate milestone; WASI 1.0 is the next. Wado tracks the spec closely.
- **Wasm Component Model × Wasm GC integration.** GC types are not yet exchangeable across CM boundaries. Once they are, Wado's value semantics will compose naturally with any CM-targeting language.

When these land, Wado's era begins.

---

## Resources

- **[Cheatsheet](./cheatsheet.md)** — Quick syntax reference
- **[Language Specification](./spec.md)** — Full language reference
- **[Compiler Internals](./compiler.md)** — How the compiler works, feature checklist
- **[Wado Evolution Proposals](./)** — Design documents (`wep-*.md`)
- **[Benchmarks](../benchmark/README.md)** — Runtime performance comparisons
- **[Wasm Size](../wasm-size/README.md)** — Binary size comparisons
- **[Repository](https://github.com/wado-lang/wado)** — Source, issues, releases

---

## License

MIT. Copyright (c) 2026, FUJI Goro (a.k.a. gfx). See [LICENSE](../LICENSE).
