//! Component Model canonical built-ins and the payload types that parameterize
//! them.
//!
//! `canon future.read` (and its siblings) are typed: the Component Model
//! instantiates one per `future<T>`, so the core module needs a distinct import
//! per payload type. [`CanonicalIntrinsic`] is that identity, carried
//! structurally from synthesis through TIR and NIR into WIR.
//!
//! [`CanonicalIntrinsic::import_name`] renders the identity as the core import
//! name at the end of that path. It is a rendering, not a carrier: nothing
//! parses it back to recover the payload, so it only has to be injective.

use std::borrow::Cow;
use std::fmt;

/// A CM (Component Model) scalar value type for parameterized canonical intrinsics.
///
/// Used to specify the element type of `future<T>` and (in the future) `stream<T>`.
/// Maps 1:1 to `wasm_encoder::PrimitiveValType` in codegen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CmScalarType {
    S8,
    S16,
    S32,
    S64,
    U8,
    U16,
    U32,
    U64,
    F32,
    F64,
    Bool,
    Char,
}

impl fmt::Display for CmScalarType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::S8 => write!(f, "s8"),
            Self::S16 => write!(f, "s16"),
            Self::S32 => write!(f, "s32"),
            Self::S64 => write!(f, "s64"),
            Self::U8 => write!(f, "u8"),
            Self::U16 => write!(f, "u16"),
            Self::U32 => write!(f, "u32"),
            Self::U64 => write!(f, "u64"),
            Self::F32 => write!(f, "float32"),
            Self::F64 => write!(f, "float64"),
            Self::Bool => write!(f, "bool"),
            Self::Char => write!(f, "char"),
        }
    }
}

impl CmScalarType {
    /// Parse the kebab-case CM scalar name produced by `Display` (the inverse).
    pub fn from_cm_name(name: &str) -> Option<Self> {
        Some(match name {
            "s8" => Self::S8,
            "s16" => Self::S16,
            "s32" => Self::S32,
            "s64" => Self::S64,
            "u8" => Self::U8,
            "u16" => Self::U16,
            "u32" => Self::U32,
            "u64" => Self::U64,
            "float32" => Self::F32,
            "float64" => Self::F64,
            "bool" => Self::Bool,
            "char" => Self::Char,
            _ => return None,
        })
    }
}

/// A general Component Model value type carried as a `future<T>` / `stream<T>`
/// payload. Self-contained (built from the type table, no registry needed), so
/// it is both a stable dedup key and a structural descriptor codegen turns into
/// a component-level type. Named types (record / variant / enum / flags /
/// resource) are referenced by their CM kebab name, already registered at the
/// component level because they appear in the world's export/import signatures.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum CmPayloadType {
    /// A primitive scalar (`bool`, integers, floats, `char`).
    Scalar(CmScalarType),
    String,
    List(Box<CmPayloadType>),
    Option(Box<CmPayloadType>),
    /// `result<ok?, err?>` — either arm absent (`None`) for the unit case.
    Result(Option<Box<CmPayloadType>>, Option<Box<CmPayloadType>>),
    Tuple(Vec<CmPayloadType>),
    /// A named CM type by kebab name (record / variant / enum / flags).
    Named(String),
    /// An owned resource handle (`own<r>`), by the resource's CM kebab name.
    /// Distinct from [`Self::Named`]: a resource's component type is registered
    /// under its own key and is wrapped in `own` at the use site.
    Resource(String),
}

impl CmPayloadType {
    /// Encode as a canonical kebab suffix used in the core import name (and the
    /// component type's debug name). Injective, so distinct CM types get
    /// distinct intrinsic imports. Inverse of [`Self::parse_suffix`].
    pub fn name_suffix(&self) -> String {
        match self {
            Self::Scalar(s) => s.to_string(),
            Self::String => "string".to_string(),
            Self::List(t) => format!("list<{}>", t.name_suffix()),
            Self::Option(t) => format!("option<{}>", t.name_suffix()),
            Self::Result(ok, err) => format!(
                "result<{},{}>",
                ok.as_ref().map_or("_".to_string(), |t| t.name_suffix()),
                err.as_ref().map_or("_".to_string(), |t| t.name_suffix()),
            ),
            Self::Tuple(elems) => format!(
                "tuple<{}>",
                elems
                    .iter()
                    .map(Self::name_suffix)
                    .collect::<Vec<_>>()
                    .join(","),
            ),
            Self::Named(name) => name.clone(),
            Self::Resource(name) => format!("own<{name}>"),
        }
    }

    /// Parse the kebab suffix produced by [`Self::name_suffix`].
    pub fn parse_suffix(s: &str) -> Option<Self> {
        let s = s.trim();
        if let Some(inner) = s.strip_prefix("list<").and_then(|r| r.strip_suffix('>')) {
            return Some(Self::List(Box::new(Self::parse_suffix(inner)?)));
        }
        if let Some(inner) = s.strip_prefix("option<").and_then(|r| r.strip_suffix('>')) {
            return Some(Self::Option(Box::new(Self::parse_suffix(inner)?)));
        }
        if let Some(inner) = s.strip_prefix("result<").and_then(|r| r.strip_suffix('>')) {
            let parts = split_top_level(inner);
            if parts.len() != 2 {
                return None;
            }
            let arm = |p: &str| -> Option<Option<Box<Self>>> {
                if p == "_" {
                    Some(None)
                } else {
                    Some(Some(Box::new(Self::parse_suffix(p)?)))
                }
            };
            return Some(Self::Result(arm(&parts[0])?, arm(&parts[1])?));
        }
        if let Some(inner) = s.strip_prefix("tuple<").and_then(|r| r.strip_suffix('>')) {
            let elems = split_top_level(inner)
                .iter()
                .map(|p| Self::parse_suffix(p))
                .collect::<Option<Vec<_>>>()?;
            return Some(Self::Tuple(elems));
        }
        if let Some(inner) = s.strip_prefix("own<").and_then(|r| r.strip_suffix('>')) {
            return Some(Self::Resource(inner.to_string()));
        }
        if s == "string" {
            return Some(Self::String);
        }
        if let Some(scalar) = CmScalarType::from_cm_name(s) {
            return Some(Self::Scalar(scalar));
        }
        // A bare kebab identifier names a record / variant / enum / flags.
        if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            return Some(Self::Named(s.to_string()));
        }
        None
    }
}

/// Split a comma-separated type list at the top nesting level only, so that
/// `result<u32, string>, list<u32>` splits into the two intended parts.
fn split_top_level(s: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut start = 0usize;
    for (i, c) in s.char_indices() {
        match c {
            '<' => depth += 1,
            '>' => depth -= 1,
            ',' if depth == 0 => {
                parts.push(s[start..i].trim().to_string());
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(s[start..].trim().to_string());
    parts
}

/// The element type of a CM `stream<T>` canonical intrinsic.
///
/// Distinguishes between distinct stream types at the Component Model level:
/// - `U8` = `stream<u8>` (default for file I/O, stdin/stdout)
/// - `Record(name)` = `stream<T>` where T is a CM record type (e.g., directory-entry)
/// - `Value(t)` = `stream<T>` for a general scalar / aggregate element type
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum CmStreamPayload {
    /// `stream<u8>` — the default stream type
    U8,
    /// `stream<T>` where T is a CM record type, identified by CM kebab-case name
    Record(String),
    /// `stream<T>` for a general scalar or aggregate element type
    Value(CmPayloadType),
}

/// The element type of a CM `future<T>` canonical intrinsic.
///
/// Distinguishes between distinct future types at the Component Model level:
/// - `Trailers` = `future<result<option<trailers>, error-code>>` (HTTP body trailers)
/// - `Transmission` = `future<result<_, error-code>>` (HTTP transmission result)
/// - `Scalar(s)` = `future<T>` where T is a primitive scalar type
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum CmFuturePayload {
    /// The HTTP trailers pattern: `future<result<option<trailers>, error-code>>`
    Trailers,
    /// `future<result<_, error-code>>` — error-code type identified by
    /// the WASI package that defines it (e.g., "cli", "filesystem", "http").
    /// Each package's error-code is a distinct CM type, so each needs
    /// its own component-level `future<result<_, E>>` definition.
    Transmission(String),
    /// A scalar value type like `future<s32>`
    Scalar(CmScalarType),
    /// A general scalar or aggregate value type like `future<string>` or
    /// `future<list<u32>>`.
    Value(CmPayloadType),
}

/// A canonical intrinsic needed by the compiled module.
///
/// Replaces the previous string-based approach (e.g., `"future-new:s32"`) with
/// structured metadata. The future type parameter is stored as `CmFuturePayload`
/// instead of being encoded in the name string.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum CanonicalIntrinsic {
    StreamNew(CmStreamPayload),
    StreamRead(CmStreamPayload),
    StreamWrite(CmStreamPayload),
    StreamDropReadable(CmStreamPayload),
    StreamDropWritable(CmStreamPayload),
    StreamCancelRead(CmStreamPayload),
    StreamCancelWrite(CmStreamPayload),
    FutureNew(CmFuturePayload),
    FutureRead(CmFuturePayload),
    FutureWrite(CmFuturePayload),
    FutureDropReadable(CmFuturePayload),
    FutureDropWritable(CmFuturePayload),
    FutureCancelRead(CmFuturePayload),
    FutureCancelWrite(CmFuturePayload),
    WaitableSetNew,
    WaitableSetWait,
    WaitableSetPoll,
    WaitableSetDrop,
    WaitableJoin,
    SubtaskDrop,
    SubtaskCancel,
    ErrorContextNew,
    ErrorContextDebugMessage,
    ErrorContextDrop,
    /// `task.return` for an `async` world export, keyed by the export's name so
    /// each export gets a `task.return` canon typed to its own result. A `--lib`
    /// world may carry several `async` exports with distinct result types; one
    /// shared canon could not type them all.
    TaskReturn(String),
    /// `resource.drop` for an imported Component Model resource.
    /// The payload is the resource's CM name (e.g. `"request"`).
    ResourceDrop(String),
}

impl CanonicalIntrinsic {
    /// The import name used in the core Wasm module.
    ///
    /// For parameterized intrinsics, includes a type suffix (e.g., `"future-new:s32"`).
    /// This is only used for the core-level import name; component codegen uses
    /// the structured enum directly.
    pub fn import_name(&self) -> String {
        match self {
            Self::StreamNew(p) => format_stream_name("stream-new", p),
            Self::StreamRead(p) => format_stream_name("stream-read", p),
            Self::StreamWrite(p) => format_stream_name("stream-write", p),
            Self::StreamDropReadable(p) => format_stream_name("stream-drop-readable", p),
            Self::StreamDropWritable(p) => format_stream_name("stream-drop-writable", p),
            Self::StreamCancelRead(p) => format_stream_name("stream-cancel-read", p),
            Self::StreamCancelWrite(p) => format_stream_name("stream-cancel-write", p),
            Self::FutureNew(p) => format_future_name("future-new", p.clone()),
            Self::FutureRead(p) => format_future_name("future-read", p.clone()),
            Self::FutureWrite(p) => format_future_name("future-write", p.clone()),
            Self::FutureDropReadable(p) => format_future_name("future-drop-readable", p.clone()),
            Self::FutureDropWritable(p) => format_future_name("future-drop-writable", p.clone()),
            Self::FutureCancelRead(p) => format_future_name("future-cancel-read", p.clone()),
            Self::FutureCancelWrite(p) => format_future_name("future-cancel-write", p.clone()),
            Self::WaitableSetNew => "waitable-set-new".to_string(),
            Self::WaitableSetWait => "waitable-set-wait".to_string(),
            Self::WaitableSetPoll => "waitable-set-poll".to_string(),
            Self::WaitableSetDrop => "waitable-set-drop".to_string(),
            Self::WaitableJoin => "waitable-join".to_string(),
            Self::SubtaskDrop => "subtask-drop".to_string(),
            Self::SubtaskCancel => "subtask-cancel".to_string(),
            Self::ErrorContextNew => "error-context-new".to_string(),
            Self::ErrorContextDebugMessage => "error-context-debug-message".to_string(),
            Self::ErrorContextDrop => "error-context-drop".to_string(),
            Self::TaskReturn(key) if key.is_empty() => "task-return".to_string(),
            Self::TaskReturn(key) => format!("task-return:{key}"),
            Self::ResourceDrop(name) => format!("resource-drop:{name}"),
        }
    }

    /// Parse a canonical intrinsic from a WASI import name.
    ///
    /// Used for TIR-level WASI imports registered before WIR translation,
    /// including the payload-parameterized stream / future intrinsics emitted
    /// as `CmRawCall`s by synthesis (e.g. `"future-read:transmission-http"`).
    /// Inverse of [`Self::import_name`].
    pub fn from_import_name(name: &str) -> Option<Self> {
        Some(match name {
            _ if name.starts_with("stream-") => {
                return parse_stream_intrinsic(name);
            }
            _ if name.starts_with("future-") => {
                return parse_future_intrinsic(name);
            }
            _ if name.starts_with("resource-drop:") => {
                Self::ResourceDrop(name["resource-drop:".len()..].to_string())
            }
            "waitable-set-new" => Self::WaitableSetNew,
            "waitable-set-wait" => Self::WaitableSetWait,
            "waitable-set-poll" => Self::WaitableSetPoll,
            "waitable-set-drop" => Self::WaitableSetDrop,
            "waitable-join" => Self::WaitableJoin,
            "subtask-drop" => Self::SubtaskDrop,
            "subtask-cancel" => Self::SubtaskCancel,
            "error-context-new" => Self::ErrorContextNew,
            "error-context-debug-message" => Self::ErrorContextDebugMessage,
            "error-context-drop" => Self::ErrorContextDrop,
            "task-return" => Self::TaskReturn(String::new()),
            _ if name.starts_with("task-return:") => {
                Self::TaskReturn(name["task-return:".len()..].to_string())
            }
            _ => return None,
        })
    }

    /// Extract the future payload type, if this is a future intrinsic.
    pub fn future_payload(&self) -> Option<CmFuturePayload> {
        match self {
            Self::FutureNew(p)
            | Self::FutureRead(p)
            | Self::FutureWrite(p)
            | Self::FutureDropReadable(p)
            | Self::FutureDropWritable(p)
            | Self::FutureCancelRead(p)
            | Self::FutureCancelWrite(p) => Some(p.clone()),
            _ => None,
        }
    }

    /// Extract the stream payload type, if this is a stream intrinsic.
    pub fn stream_payload(&self) -> Option<CmStreamPayload> {
        match self {
            Self::StreamNew(p)
            | Self::StreamRead(p)
            | Self::StreamWrite(p)
            | Self::StreamDropReadable(p)
            | Self::StreamDropWritable(p)
            | Self::StreamCancelRead(p)
            | Self::StreamCancelWrite(p) => Some(p.clone()),
            _ => None,
        }
    }
}

fn parse_stream_intrinsic(name: &str) -> Option<CanonicalIntrinsic> {
    // Parse "stream-read:directory-entry" → StreamRead(Record("directory-entry"))
    // Parse "stream-read" → StreamRead(U8)
    let (base, payload) = if let Some((b, suffix)) = name.split_once(':') {
        let payload = if let Some(val) = suffix.strip_prefix("val-") {
            CmStreamPayload::Value(CmPayloadType::parse_suffix(val)?)
        } else {
            CmStreamPayload::Record(suffix.to_string())
        };
        (b, payload)
    } else {
        (name, CmStreamPayload::U8)
    };
    Some(match base {
        "stream-new" => CanonicalIntrinsic::StreamNew(payload),
        "stream-read" => CanonicalIntrinsic::StreamRead(payload),
        "stream-write" => CanonicalIntrinsic::StreamWrite(payload),
        "stream-drop-readable" => CanonicalIntrinsic::StreamDropReadable(payload),
        "stream-drop-writable" => CanonicalIntrinsic::StreamDropWritable(payload),
        "stream-cancel-read" => CanonicalIntrinsic::StreamCancelRead(payload),
        "stream-cancel-write" => CanonicalIntrinsic::StreamCancelWrite(payload),
        _ => return None,
    })
}

fn parse_future_intrinsic(name: &str) -> Option<CanonicalIntrinsic> {
    // Partial inverse of `format_future_name`:
    //   "future-read:transmission-http"   → FutureRead(Transmission("http"))
    //   "future-read:s32"                 → FutureRead(Scalar(S32))
    //
    // A suffix-less name carries no payload, so it does not parse. It is what a
    // `#[canonical("wasi", "future-read")]` annotation spells — a template the
    // call site parameterizes — and reading it as the trailers future (which
    // renders to that same bare name) would answer a payload nobody stated.
    let (base, payload) = match name.split_once(':') {
        None => return None,
        Some((b, suffix)) => {
            let payload = if let Some(source) = suffix.strip_prefix("transmission-") {
                CmFuturePayload::Transmission(source.to_string())
            } else if let Some(val) = suffix.strip_prefix("val-") {
                CmFuturePayload::Value(CmPayloadType::parse_suffix(val)?)
            } else {
                CmFuturePayload::Scalar(CmScalarType::from_cm_name(suffix)?)
            };
            (b, payload)
        }
    };
    Some(match base {
        "future-new" => CanonicalIntrinsic::FutureNew(payload),
        "future-read" => CanonicalIntrinsic::FutureRead(payload),
        "future-write" => CanonicalIntrinsic::FutureWrite(payload),
        "future-drop-readable" => CanonicalIntrinsic::FutureDropReadable(payload),
        "future-drop-writable" => CanonicalIntrinsic::FutureDropWritable(payload),
        "future-cancel-read" => CanonicalIntrinsic::FutureCancelRead(payload),
        "future-cancel-write" => CanonicalIntrinsic::FutureCancelWrite(payload),
        _ => return None,
    })
}

fn format_stream_name(base: &str, payload: &CmStreamPayload) -> String {
    match payload {
        CmStreamPayload::U8 => base.to_string(),
        CmStreamPayload::Record(name) => format!("{base}:{name}"),
        CmStreamPayload::Value(t) => format!("{base}:val-{}", t.name_suffix()),
    }
}

fn format_future_name(base: &str, payload: CmFuturePayload) -> String {
    match payload {
        CmFuturePayload::Trailers => base.to_string(),
        CmFuturePayload::Transmission(ref source) => format!("{base}:transmission-{source}"),
        CmFuturePayload::Scalar(scalar) => format!("{base}:{scalar}"),
        CmFuturePayload::Value(ref t) => format!("{base}:val-{}", t.name_suffix()),
    }
}
/// What a `CmRawCall` invokes.
///
/// Synthesis knows which of the two it is emitting, so it says so here rather
/// than encoding it into a name a later phase has to parse back. Recovering a
/// [`CanonicalIntrinsic`] from its rendered name is lossy — the trailers future
/// renders to the bare base name, which is also what an unclassified payload
/// produced — and that ambiguity is what this type removes.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum CmCallTarget {
    /// A lowered WASI import, by its local alias name
    /// (e.g. `"wasi:cli/stdout@0.3.0/write-via-stream"`).
    WasiAlias(String),
    /// A Component Model canonical built-in, carried by identity.
    Canonical(CanonicalIntrinsic),
}

impl CmCallTarget {
    /// The core-module import name this target resolves to.
    pub fn import_name(&self) -> Cow<'_, str> {
        match self {
            Self::WasiAlias(name) => Cow::Borrowed(name),
            Self::Canonical(intrinsic) => Cow::Owned(intrinsic.import_name()),
        }
    }

    /// The canonical intrinsic this target invokes, if it is one.
    pub fn canonical(&self) -> Option<&CanonicalIntrinsic> {
        match self {
            Self::WasiAlias(_) => None,
            Self::Canonical(intrinsic) => Some(intrinsic),
        }
    }
}

#[cfg(test)]
mod intrinsic_name_tests {
    use super::*;

    fn round_trip(intr: CanonicalIntrinsic) {
        let name = intr.import_name();
        assert_eq!(
            CanonicalIntrinsic::from_import_name(&name),
            Some(intr),
            "round-trip failed for {name:?}"
        );
    }

    /// The trailers future renders to the bare base name, which is also what a
    /// payload-parameterized `#[canonical]` annotation spells. Parsing must not
    /// resolve that to a payload — answering `Trailers` there is what made an
    /// unclassified `future<T>` come back as the HTTP trailers future.
    #[test]
    fn a_bare_future_name_does_not_parse_to_a_payload() {
        for name in [
            "future-new",
            "future-read",
            "future-write",
            "future-drop-readable",
            "future-drop-writable",
            "future-cancel-read",
            "future-cancel-write",
        ] {
            assert_eq!(
                CanonicalIntrinsic::from_import_name(name),
                None,
                "`{name}` states no payload, so it must not parse"
            );
        }
    }

    /// Every payload-carrying name round-trips. `Trailers` is deliberately
    /// absent: it renders to the bare name, which no longer parses back.
    #[test]
    fn future_intrinsics_round_trip() {
        for base in [
            CanonicalIntrinsic::FutureNew as fn(CmFuturePayload) -> CanonicalIntrinsic,
            CanonicalIntrinsic::FutureRead,
            CanonicalIntrinsic::FutureWrite,
            CanonicalIntrinsic::FutureDropReadable,
            CanonicalIntrinsic::FutureDropWritable,
            CanonicalIntrinsic::FutureCancelRead,
            CanonicalIntrinsic::FutureCancelWrite,
        ] {
            round_trip(base(CmFuturePayload::Transmission("http".to_string())));
            round_trip(base(CmFuturePayload::Transmission(
                "filesystem".to_string(),
            )));
            round_trip(base(CmFuturePayload::Transmission("cli".to_string())));
            round_trip(base(CmFuturePayload::Scalar(CmScalarType::S32)));
            round_trip(base(CmFuturePayload::Scalar(CmScalarType::F64)));
            round_trip(base(CmFuturePayload::Scalar(CmScalarType::Bool)));
        }
    }

    #[test]
    fn scalar_name_round_trips() {
        for s in [
            CmScalarType::S8,
            CmScalarType::S16,
            CmScalarType::S32,
            CmScalarType::S64,
            CmScalarType::U8,
            CmScalarType::U16,
            CmScalarType::U32,
            CmScalarType::U64,
            CmScalarType::F32,
            CmScalarType::F64,
            CmScalarType::Bool,
            CmScalarType::Char,
        ] {
            assert_eq!(CmScalarType::from_cm_name(&s.to_string()), Some(s));
        }
    }
}
