//! Component Model canonical built-ins and their payload types.
//!
//! `canon future.read` and its siblings are typed — one per `future<T>` — so
//! the core module needs a distinct import per payload. [`CanonicalIntrinsic`]
//! is that identity, carried from synthesis through TIR and NIR into WIR;
//! [`CanonicalIntrinsic::import_name`] renders it at the end of that path.

use std::borrow::Cow;
use std::fmt;

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

/// A Component Model value type carried as a `future<T>` / `stream<T>` payload.
/// Self-contained — no registry needed — so it doubles as a dedup key.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum CmPayloadType {
    Scalar(CmScalarType),
    String,
    List(Box<CmPayloadType>),
    Option(Box<CmPayloadType>),
    Result(Option<Box<CmPayloadType>>, Option<Box<CmPayloadType>>),
    Tuple(Vec<CmPayloadType>),
    /// A record / variant / enum / flags, by CM kebab name.
    Named(String),
    /// An owned resource handle, by the resource's CM kebab name. Separate from
    /// [`Self::Named`]: its component type is keyed differently and wrapped in
    /// `own` at the use site.
    Resource(String),
}

impl CmPayloadType {
    /// Injective, so distinct CM types get distinct imports.
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

    pub fn parse_suffix(s: &str) -> Option<Self> {
        let s = s.trim();
        if let Some(inner) = s.strip_circumfix("list<", ">") {
            return Some(Self::List(Box::new(Self::parse_suffix(inner)?)));
        }
        if let Some(inner) = s.strip_circumfix("option<", ">") {
            return Some(Self::Option(Box::new(Self::parse_suffix(inner)?)));
        }
        if let Some(inner) = s.strip_circumfix("result<", ">") {
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
        if let Some(inner) = s.strip_circumfix("tuple<", ">") {
            let elems = split_top_level(inner)
                .iter()
                .map(|p| Self::parse_suffix(p))
                .collect::<Option<Vec<_>>>()?;
            return Some(Self::Tuple(elems));
        }
        if let Some(inner) = s.strip_circumfix("own<", ">") {
            return Some(Self::Resource(inner.to_string()));
        }
        if s == "string" {
            return Some(Self::String);
        }
        if let Some(scalar) = CmScalarType::from_cm_name(s) {
            return Some(Self::Scalar(scalar));
        }
        if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            return Some(Self::Named(s.to_string()));
        }
        None
    }
}

/// Splits at the top nesting level only, so `result<u32,string>,list<u32>`
/// splits in two.
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

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum CmStreamPayload {
    /// The default stream, and the only suffix-less one.
    U8,
    /// A CM record element, by its kebab name.
    Record(String),
    Value(CmPayloadType),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum CmFuturePayload {
    /// `future<result<option<trailers>, error-code>>`.
    Trailers,
    /// `future<result<_, error-code>>`, keyed by the WASI package defining the
    /// error-code (`"cli"`, `"filesystem"`, …) — each is a distinct CM type.
    Transmission(String),
    Scalar(CmScalarType),
    Value(CmPayloadType),
}

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
    /// Keyed by the export's name: a `--lib` world may carry several with
    /// distinct result types, which one shared canon could not type.
    TaskReturn(String),
    /// By the resource's CM name.
    ResourceDrop(String),
}

impl CanonicalIntrinsic {
    /// The core-module import name, as in `"future-new:s32"`.
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

    /// Parse the name a `#[canonical("wasi", "...")]` annotation spells. Only a
    /// partial inverse of [`Self::import_name`]: a name stating no payload does
    /// not parse.
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

/// A suffix-less name carries no payload, so it does not parse — unlike the
/// stream side, where it is the default `stream<u8>`.
fn parse_future_intrinsic(name: &str) -> Option<CanonicalIntrinsic> {
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
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum CmCallTarget {
    /// A lowered WASI import, by its local alias name
    /// (`"wasi:cli/stdout@0.3.0/write-via-stream"`).
    WasiAlias(String),
    Canonical(CanonicalIntrinsic),
}

impl CmCallTarget {
    pub fn import_name(&self) -> Cow<'_, str> {
        match self {
            Self::WasiAlias(name) => Cow::Borrowed(name),
            Self::Canonical(intrinsic) => Cow::Owned(intrinsic.import_name()),
        }
    }

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

    /// `Trailers` is deliberately absent: it renders to the bare name, which
    /// the test above pins as unparsable.
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
