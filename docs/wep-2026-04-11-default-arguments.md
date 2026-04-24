# WEP: Default Arguments

## Context

Wado currently requires all function arguments and all struct fields to be specified explicitly at every call site and construction site. This creates verbosity in two major areas.

### WebIDL Bindings

The [WebIDL Binding Generator](./wep-2026-04-01-tide.md) generates Wado bindings for browser APIs. WebIDL makes heavy use of optional parameters with default values:

```webidl
Node cloneNode(optional boolean deep = false);
undefined addEventListener(DOMString type, EventListener? callback,
                           optional AddEventListenerOptions options);
undefined fillText(DOMString text, double x, double y, optional double maxWidth);
```

Without default arguments, these map to `Option<T>` parameters, which is verbose and introduces runtime overhead (Option wrapping/unwrapping):

```wado
// Current: Option<T> mapping
fn clone_node(&self, deep: Option<bool>) -> Node;
fn fill_text(&self, text: String, x: f64, y: f64, max_width: Option<f64>);

node.clone_node(Option::<bool>::Some(false));
ctx.fill_text("Hello", 50.0, 150.0, null);
```

WebIDL dictionaries have the same problem. All dictionary members are optional:

```webidl
dictionary RequestInit {
  ByteString method;
  BodyInit? body;
  RequestMode mode;
};
```

```wado
// Current: all fields are Option<T>
let init = RequestInit {
    method: Option::<String>::Some("POST"),
    body: Option::<String>::Some(data),
    headers: null,
    mode: null,
    credentials: null,
};
```

### General Ergonomics

Beyond WebIDL, many APIs have parameters with natural default values:

```wado
// Without defaults: multiple function names or Option wrapping
fn connect(host: String, port: i32, timeout: i32) { ... }
fn connect_default(host: String) { connect(host, 8080, 30); }
```

### Design Requirements

- Zero overhead: a call with defaults omitted must produce the same Wasm as a call with all arguments explicit.
- Simple, uniform rules for both function parameters and struct fields.

### Cross-Language Survey

| Language | Approach               | Evaluation          | Named Args       |
| -------- | ---------------------- | ------------------- | ---------------- |
| Swift    | Inline defaults        | Call-site           | Yes (labels)     |
| Kotlin   | Inline defaults        | Call-site (bitmask) | Yes              |
| Zig      | Struct field defaults  | Construction-site   | No (field names) |
| Python   | Inline defaults        | Definition-time     | Yes              |
| C++      | Inline defaults        | Call-site           | No               |
| Rust     | None (builder pattern) | —                   | No               |

Key lessons:

- Call-site evaluation avoids Python's mutable-default pitfall.
- Named fields/arguments make implicit omission safe (Zig, Swift, Kotlin).
- Keeping defaults out of trait/protocol requirements avoids dispatch complexity (Swift).
- Kotlin's bitmask approach is ABI-stable but has runtime overhead.
- Zig's struct-field-default pattern is zero overhead and works without named arguments.

## Decision

### Function Parameter Defaults

Trailing parameters may have default values:

```wado
fn connect(host: String, port: i32 = 8080, timeout: i32 = 30) { ... }

connect("localhost");           // → connect("localhost", 8080, 30)
connect("localhost", 3000);     // → connect("localhost", 3000, 30)
connect("localhost", 3000, 60); // no desugaring
```

#### Syntax

```
fn name(param: Type, param: Type = expr, ...) -> ReturnType { ... }
```

Parameters with defaults must come after all parameters without defaults. This is enforced at parse time.

```wado
fn foo(a: i32 = 0, b: i32) { ... }  // compile error: non-default param after default
```

The `self` parameter cannot have a default.

#### Semantics: Call-Site Expansion

The compiler inserts default values at the call site as a desugaring pass. This is purely a compile-time transformation — no runtime cost.

```wado
fn greet(name: String, greeting: String = "Hello") -> String {
    return `{greeting}, {name}!`;
}

// Source:
greet("Alice");
// After desugaring (identical Wasm):
greet("Alice", "Hello");
```

#### Default Expressions

Any expression without effects is allowed as a default:

```wado
fn foo(
    x: i32 = 0,                      // literal
    y: f64 = f64::PI,                 // associated constant
    z: String = `default`,            // template string
    w: Option<Config> = null,         // null (coerces to Option::None)
    v: Color = Color::Red,            // enum variant
    u: i32 = i32::max(1, 2),          // pure function call
) { ... }

// Error: effectful expression in default
fn bar(path: String = read_env("PATH")) { ... }  // compile error: requires effects
```

The restriction is enforced by the effect system: default expressions must have an empty effect set.

Default expressions may reference earlier parameters in the same function:

```wado
fn make_rect(width: f64, height: f64 = width) -> Rect { ... }

make_rect(10.0);        // → make_rect(10.0, 10.0)
make_rect(10.0, 20.0);
```

#### Interaction with Function Types

Function types do not carry default information. Assigning a function with defaults to a function type erases the defaults:

```wado
fn connect(host: String, port: i32 = 8080) { ... }

let f: fn(String, i32) = connect;  // OK: full signature
f("localhost", 3000);              // OK
f("localhost");                    // compile error: expected 2 arguments, found 1

// The arity is part of the type — no defaults in fn types
let g: fn(String) = connect;      // compile error: type mismatch
```

This is a direct consequence of zero overhead: function types represent the actual call signature at the Wasm level.

#### Interaction with Closures

Closures cannot declare default parameters. Closures are first-class values whose type is `fn(...)`, which carries arity but not defaults. Any call site of a closure must supply every argument.

```wado
// Compile error: closure parameters cannot have defaults.
let greet = |name: String, greeting: String = "Hello"| `{greeting}, {name}!`;
```

The parser still accepts the `= expr` syntax for closure parameters so that recovery is uniform with named functions, but the resolver rejects it with an error that points at the default expression. Use a wrapper function with defaults instead:

```wado
fn greet(name: String, greeting: String = "Hello") -> String {
    return `{greeting}, {name}!`;
}
```

Rationale: closures erase defaults the instant they are assigned to a `fn(...)` variable, so permitting defaults at the closure literal site would create inconsistent call-site behavior (same closure, different call sites, different argument counts) and would imply a non-trivial desugaring for captured state. Keeping defaults out of closures preserves the invariant that "a closure value and its type agree on arity".

#### Interaction with Trait Methods

Only the trait definition may specify default arguments. Implementations cannot add, remove, or change defaults:

```wado
trait Drawable {
    fn draw(&self, opacity: f64 = 1.0);
}

impl Drawable for Circle {
    fn draw(&self, opacity: f64) {
        // impl receives all parameters — no default syntax here
    }
}

let c = Circle { radius: 5.0 };
c.draw();      // → c.draw(1.0)   — trait's default applied at call site
c.draw(0.5);
```

Rationale: the caller sees the trait's interface, so the trait owns the defaults. This avoids ambiguity about which defaults apply when calling through a trait reference, and prevents implementations from silently changing call-site behavior.

If a method is not part of a trait (a direct `impl` method), it may freely define defaults:

```wado
impl Circle {
    fn scale(&mut self, factor: f64 = 2.0) {
        self.radius = self.radius * factor;
    }
}
```

#### Interaction with Component Model Exports

`export fn` cannot declare default parameters. Exported functions appear in the component's public interface (WIT), where every parameter is required by the CM ABI. Permitting defaults only on the Wado side would create a divergence between the WIT signature and the in-language signature that must be reconciled at every tool boundary (`wado doc`, IDE hover, CM host bindings). Rather than carry that inconsistency, Wado rejects the default outright.

```wado
// Compile error: export fn cannot have default parameters.
export fn configure(name: String, debug: bool = false) { ... }
```

The parser still accepts the syntax so that the error message can point at the default expression. The fix is either to drop the default, or to split into a private helper with the default and a thin `export fn` wrapper:

```wado
fn configure_impl(name: String, debug: bool = false) { ... }

export fn configure(name: String, debug: bool) {
    configure_impl(name, debug);
}
```

### Struct Field Defaults

Struct fields may have default values:

```wado
struct Config {
    host: String,
    port: i32 = 8080,
    timeout: i32 = 30,
    debug: bool = false,
}
```

#### Implicit Omission

Fields with defaults may be omitted in struct construction. Fields without defaults are required:

```wado
let c = Config { host: "localhost" };
// Desugars to: Config { host: "localhost", port: 8080, timeout: 30, debug: false }

let c = Config { host: "localhost", port: 3000 };
// Desugars to: Config { host: "localhost", port: 3000, timeout: 30, debug: false }

Config { port: 3000 };  // compile error: missing required field 'host'
```

No special syntax marker (such as `..`) is needed. This follows the same model as Zig, Swift, Kotlin, Dart, and Scala.

Omission is safe because struct construction uses named fields — the compiler checks that all provided field names are valid and all required fields are present.

#### Default Expressions

Same rules as function parameter defaults: any expression without effects.

```wado
struct Circle {
    center: Point = Point { x: 0, y: 0 },
    radius: f64 = 1.0,
}

let unit_circle = Circle {};  // center at origin, radius 1.0
```

Unlike function parameter defaults, struct field default expressions cannot reference other fields.

#### Interaction with Shorthand Construction

Field shorthand (variable name matches field name) works with defaults:

```wado
let host = "localhost";
let c = Config { host };
// Desugars to: Config { host: host, port: 8080, timeout: 30, debug: false }
```

#### Interaction with Destructuring

Defaults do not affect destructuring. When destructuring, all fields are available:

```wado
let Config { host, port, timeout, debug } = config;  // all fields accessible
let Config { host, .. } = config;                     // ignore rest
```

#### Interaction with Default Trait

A struct where all fields have defaults automatically implements `Default`:

```wado
struct Config {
    host: String = "localhost",
    port: i32 = 8080,
}

// Auto-derived (the compiler synthesizes this):
// impl Default for Config {
//     fn default() -> Config {
//         return Config { host: "localhost", port: 8080 };
//     }
// }

let c = Config::default();
```

A struct with any required field (no default) does not auto-derive `Default`.

#### Interaction with Serde

The `#[serde(default)]` attribute on a struct field currently uses `Default` trait for the field's type. With struct field defaults, `#[serde(default)]` uses the field's declared default value instead:

```wado
struct Config {
    #[serde(default)]
    port: i32 = 8080,  // serde uses 8080 when field is missing from JSON
}
```

### WebIDL Mapping Improvement

With default arguments, WebIDL optional parameters with explicit defaults map to their actual types instead of `Option<T>`:

```wado
// WebIDL: optional boolean subtree = false
// Before (Option wrapping):
fn clone_node(&self, deep: Option<bool>) -> Node;
// After (default value from IDL):
fn clone_node(&self, deep: bool = false) -> Node;

// WebIDL: optional boolean counterclockwise = false
fn arc(&self, x: f64, y: f64, radius: f64,
       start_angle: f64, end_angle: f64, anticlockwise: bool = false);
```

WebIDL optional parameters without explicit defaults map to `Option<T>` with `= null`:

```wado
// WebIDL: optional double maxWidth (no default in IDL)
fn fill_text(&self, text: String, x: f64, y: f64, max_width: Option<f64> = null);
```

WebIDL optional parameters with `= {}` (empty dictionary default) map to struct type with struct default:

```wado
// WebIDL: optional RequestInit init = {}
fn fetch(input: String, init: RequestInit = RequestInit {}) -> Future<Response>;

// WebIDL: optional AddEventListenerOptions options = {}
fn add_event_listener(&self, type: String, callback: Option<fn(Event)>,
                      options: AddEventListenerOptions = AddEventListenerOptions {});
```

WebIDL dictionaries map to structs with field defaults. Members with explicit defaults use the declared default; members without defaults use `Option<T> = null`:

```wado
// WebIDL:
// dictionary RequestInit {
//   ByteString method;          ← no default in IDL
//   BodyInit? body;             ← no default in IDL
//   RequestMode mode;           ← no default in IDL
// };

// Before:
pub struct RequestInit {
    pub method: Option<String>,
    pub body: Option<String>,
    pub mode: Option<RequestMode>,
}

// After: all fields Option<T> = null (no IDL defaults to use)
pub struct RequestInit {
    pub method: Option<String> = null,
    pub body: Option<String> = null,
    pub mode: Option<RequestMode> = null,
}

// Dictionary WITH explicit defaults:
// dictionary ResponseInit {
//   unsigned short status = 200;    ← default in IDL
//   ByteString statusText = "";     ← default in IDL
//   HeadersInit headers;            ← no default
// };
pub struct ResponseInit {
    pub status: u16 = 200,
    pub status_text: String = "",
    pub headers: Option<Headers> = null,
}
```

User code becomes significantly cleaner:

```wado
// Before:
let init = RequestInit {
    method: Option::<String>::Some("POST"),
    body: Option::<String>::Some(data),
    mode: null,
};
let resp = Fetch::fetch(url, Option::Some(init)).read();

// After: struct field defaults eliminate boilerplate, fetch default is {}
let init: RequestInit = { method: Option::Some("POST"), body: Option::Some(data) };
let resp = Fetch::fetch(url, init).read();
```

## Consequences

### Benefits

- Zero runtime overhead — defaults are a compile-time desugaring.
- WebIDL bindings become ergonomic: `Option<T>` wrapping eliminated for parameters with known defaults.
- Simple rule: "has default → optional, no default → required" applies uniformly to function parameters and struct fields.
- Trait defaults are owned by the trait definition, avoiding ambiguity.

### Trade-offs

- Default value changes require recompilation of callers. This is a non-issue for Wado's whole-program compilation model.
- Defaults are not part of function types. Code that stores functions as `fn(...)` values loses access to defaults. This is intentional (zero overhead).
- No named arguments: without named arguments, only trailing parameters can have defaults. Middle parameters cannot be skipped. This limits flexibility compared to Swift/Kotlin but keeps the language simpler.

### Interaction with Existing Features

| Feature          | Interaction                                         |
| ---------------- | --------------------------------------------------- |
| Effect system    | Default expressions must be pure (no effects)       |
| Traits           | Only trait definition specifies defaults            |
| Function types   | Defaults erased — arity fixed                       |
| Closures         | Defaults rejected — parsed but error in resolver    |
| `export fn` (CM) | Defaults rejected — arity must match WIT signature  |
| Default trait    | Auto-derived for all-defaulted structs              |
| Serde            | `#[serde(default)]` uses field default value        |
| Generics         | Default expressions are monomorphized per call site |

### Not in Scope

- Named arguments — a separate feature that would complement defaults by allowing middle-parameter skipping.
- Struct update syntax (`{ field: value, ..other }`) — copying fields from another instance is orthogonal to field defaults.

## Implementation Roadmap

- [x] Parameter and struct-field defaults end-to-end (AST / parser / resolver / TIR / trait propagation / purity check). Covered by the `default_arg_*` and `default_field_*` fixtures in `wado-compiler/tests/fixtures/`.
- [x] Auto-derive `Default` for all-defaulted non-generic structs. Generic structs are a follow-up.
- [ ] `wado-from-idl`: emit `= default` instead of `Option<T>` for WebIDL parameters with IDL defaults, and map WebIDL dictionaries to structs with field defaults. Blocked on WebIDL support in `wado-from-idl` itself (currently WIT-only).

## See Also

- [Default Trait](./wep-2026-03-04-default-trait.md) — the `Default` trait that interacts with struct field defaults
- [WebIDL Binding Generator](./wep-2026-04-01-tide.md) — the primary motivation for this feature
- [Effect System Design](./wep-2026-01-27-effect-system-design.md) — enforces purity of default expressions
