//! Component Model canonical built-ins and their payload types.
//!
//! `canon future.read` and its siblings are typed — one per `future<T>` — so
//! the core module needs a distinct import per payload. [`CanonicalIntrinsic`]
//! is that identity, carried from synthesis through TIR and NIR into WIR;
//! [`CanonicalIntrinsic::import_name`] renders it at the end of that path.

use std::borrow::Cow;
use std::fmt;

use crate::defs::{DefId, DefTable};
use crate::module_source::ModuleSource;

/// A Component Model type some declaration names: the identity every key
/// compares, beside the CM name the ABI spells.
///
/// The CM name is a rendering and never a way back. A CM name alone puts every
/// package in one namespace, and its package puts every interface in one.
#[derive(Debug, Clone)]
pub struct CmDecl {
    def: DefId,
    module: ModuleSource,
    cm_name: String,
}

impl PartialEq for CmDecl {
    fn eq(&self, other: &Self) -> bool {
        self.def == other.def
    }
}

impl Eq for CmDecl {}

impl std::hash::Hash for CmDecl {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.def.hash(state);
    }
}

impl CmDecl {
    /// `def` under the CM name the ABI spells it by. The declaring module comes
    /// off the table, never from a caller.
    #[must_use]
    pub fn new(defs: &DefTable, def: DefId, cm_name: &str) -> Self {
        Self {
            def,
            module: defs.module(def).clone(),
            cm_name: cm_name.to_string(),
        }
    }

    #[must_use]
    pub fn def(&self) -> DefId {
        self.def
    }

    /// The name the Component Model ABI spells this type by.
    #[must_use]
    pub fn cm_name(&self) -> &str {
        &self.cm_name
    }

    #[must_use]
    pub fn module(&self) -> &ModuleSource {
        &self.module
    }

    /// The injective spelling a canonical import name embeds.
    #[must_use]
    pub fn name_suffix(&self) -> String {
        format!("{}#{}", self.module.to_path_string(), self.cm_name)
    }

    /// The CM package the declaring interface sits in (`"http"`, `"cli"`, …),
    /// for a consumer scoping a name resolution to it.
    #[must_use]
    pub fn cm_package(&self) -> Option<&str> {
        match &self.module {
            ModuleSource::Binding { interface, .. } => interface.split('/').next(),
            _ => None,
        }
    }
}

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
    /// A record / variant / enum / flags, by its declaration.
    Named(CmDecl),
    /// An owned resource handle, by the resource's declaration. Separate from
    /// [`Self::Named`]: its component type is keyed differently and wrapped in
    /// `own` at the use site.
    Resource(CmDecl),
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
            Self::Named(decl) => decl.name_suffix(),
            Self::Resource(decl) => format!("own<{}>", decl.name_suffix()),
        }
    }

    /// Visit every declaration this payload reaches, each with the kind it is
    /// reached as.
    pub fn for_each_decl(&self, f: &mut impl FnMut(&CmDecl, CmDeclKind)) {
        match self {
            Self::Named(decl) => f(decl, CmDeclKind::Value),
            Self::Resource(decl) => f(decl, CmDeclKind::Resource),
            Self::List(t) | Self::Option(t) => t.for_each_decl(f),
            Self::Result(ok, err) => {
                for t in [ok, err].into_iter().flatten() {
                    t.for_each_decl(f);
                }
            }
            Self::Tuple(elems) => {
                for e in elems {
                    e.for_each_decl(f);
                }
            }
            Self::Scalar(_) | Self::String => {}
        }
    }
}

/// How a payload reaches a declaration: as a value type, or as a resource whose
/// handle the use site wraps in `own<>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmDeclKind {
    Value,
    Resource,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum CmStreamPayload {
    /// The default stream, and the only suffix-less one.
    U8,
    /// A CM record element, by its declaration.
    Record(CmDecl),
    Value(CmPayloadType),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum CmFuturePayload {
    /// `future<result<option<trailers>, error-code>>`.
    Trailers,
    /// `future<result<_, error-code>>`, by the declaration of the error-code it
    /// carries. Each is a distinct CM type, and two interfaces of one package may
    /// each declare one.
    Transmission(CmDecl),
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
    /// Keyed by the export whose result it delivers; one canon carries one
    /// result type. The empty key is the shared canon of the `result<>` ones.
    TaskReturn(String),
    /// By the resource's declaration.
    ResourceDrop(CmDecl),
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
            Self::ResourceDrop(decl) => format!("resource-drop:{}", decl.name_suffix()),
        }
    }

    /// The intrinsic a `#[canonical("wasi", "...")]` annotation names, which is
    /// always a payload-less operation.
    ///
    /// Not an inverse of [`Self::import_name`] and cannot be one: an annotation
    /// cannot spell a payload, so a name carrying one answers `None`. The call
    /// site's `Future<T>` / `Stream<T>` supplies it through
    /// [`Self::future_op`] / [`Self::stream_op`].
    pub fn from_import_name(name: &str) -> Option<Self> {
        Some(match name {
            _ if name.starts_with("stream-") => {
                return Self::stream_op(name, CmStreamPayload::U8);
            }
            _ if name.starts_with("future-") => return None,
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

    /// Whether the canonical returns a value the declaring signature discards:
    /// a cancel answers with the `u32` its copy ended on, and
    /// `fn cancel_read(&self)` states no result. The caller drops it.
    #[must_use]
    pub fn returns_discarded_result(&self) -> bool {
        matches!(
            self,
            Self::StreamCancelRead(_)
                | Self::StreamCancelWrite(_)
                | Self::FutureCancelRead(_)
                | Self::FutureCancelWrite(_)
                | Self::SubtaskCancel
        )
    }

    /// The stream intrinsic a payload-less base name denotes, at `payload`.
    /// The one place a `stream-*` name maps to its variant.
    pub fn stream_op(base: &str, payload: CmStreamPayload) -> Option<Self> {
        Some(match base {
            "stream-new" => Self::StreamNew(payload),
            "stream-read" => Self::StreamRead(payload),
            "stream-write" => Self::StreamWrite(payload),
            "stream-drop-readable" => Self::StreamDropReadable(payload),
            "stream-drop-writable" => Self::StreamDropWritable(payload),
            "stream-cancel-read" => Self::StreamCancelRead(payload),
            "stream-cancel-write" => Self::StreamCancelWrite(payload),
            _ => return None,
        })
    }

    /// The future intrinsic a payload-less base name denotes, at `payload`.
    /// The `stream_op` counterpart.
    pub fn future_op(base: &str, payload: CmFuturePayload) -> Option<Self> {
        Some(match base {
            "future-new" => Self::FutureNew(payload),
            "future-read" => Self::FutureRead(payload),
            "future-write" => Self::FutureWrite(payload),
            "future-drop-readable" => Self::FutureDropReadable(payload),
            "future-drop-writable" => Self::FutureDropWritable(payload),
            "future-cancel-read" => Self::FutureCancelRead(payload),
            "future-cancel-write" => Self::FutureCancelWrite(payload),
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

fn format_stream_name(base: &str, payload: &CmStreamPayload) -> String {
    match payload {
        CmStreamPayload::U8 => base.to_string(),
        CmStreamPayload::Record(decl) => format!("{base}:{}", decl.name_suffix()),
        CmStreamPayload::Value(t) => format!("{base}:val-{}", t.name_suffix()),
    }
}

fn format_future_name(base: &str, payload: CmFuturePayload) -> String {
    match payload {
        CmFuturePayload::Trailers => base.to_string(),
        CmFuturePayload::Transmission(ref decl) => {
            format!("{base}:transmission-{}", decl.name_suffix())
        }
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

    /// A payload is a declaration or a structure, and an annotation can spell
    /// neither, so a rendered name never travels back into one.
    #[test]
    fn a_payload_carrying_name_does_not_parse() {
        for name in [
            "future-new",
            "future-read",
            "future-write",
            "future-drop-readable",
            "future-drop-writable",
            "future-cancel-read",
            "future-cancel-write",
            "future-read:s32",
            "future-read:transmission-wasi/cli/types#error-code",
            "stream-read:wasi/filesystem/types#directory-entry",
            "resource-drop:wasi/filesystem/types#descriptor",
        ] {
            assert_eq!(
                CanonicalIntrinsic::from_import_name(name),
                None,
                "a name carrying a payload must not parse"
            );
        }
    }

    /// An annotation name states an operation and no payload. A stream one
    /// means the default `stream<u8>`.
    #[test]
    fn annotation_names_denote_their_operation() {
        assert_eq!(
            CanonicalIntrinsic::from_import_name("stream-read"),
            Some(CanonicalIntrinsic::StreamRead(CmStreamPayload::U8)),
        );
        assert_eq!(
            CanonicalIntrinsic::from_import_name("task-return"),
            Some(CanonicalIntrinsic::TaskReturn(String::new())),
        );
        assert_eq!(
            CanonicalIntrinsic::from_import_name("waitable-join"),
            Some(CanonicalIntrinsic::WaitableJoin),
        );
        assert_eq!(
            CanonicalIntrinsic::from_import_name("not-a-canonical"),
            None
        );
    }

    #[test]
    fn a_scalar_payload_renders_its_cm_name() {
        assert_eq!(
            CanonicalIntrinsic::FutureRead(CmFuturePayload::Scalar(CmScalarType::S32))
                .import_name(),
            "future-read:s32",
        );
        assert_eq!(
            CanonicalIntrinsic::StreamRead(CmStreamPayload::U8).import_name(),
            "stream-read",
        );
    }
}
