//! Every naming and mangling decision in the compiler. A method mangles as
//! `{module}/{struct}^{trait}::{method}` (the trait half optional), a struct as
//! `{module}/{name}`, a WASI name as `wasi:{package}/{interface}::{function}`.
//! `function.rs` holds the function and struct names, `type_name.rs` the type
//! names and their mangles.

mod function;
mod type_name;

pub use function::*;
pub use type_name::*;

use crate::ast;
use crate::ast::{AstId, TestMetadata};
use crate::lexer::is_ident_continue;
use crate::module_source::ModuleSource;
use crate::nir_package::{FieldForm, ReturnShape};
use crate::syntax::{CONTEXTUAL_KEYWORDS, KEYWORDS, NAME_KEYWORDS};
use heck::{ToKebabCase, ToSnakeCase, ToUpperCamelCase};
use sha2::{Digest, Sha256};
use std::fmt;
use std::fmt::Write as _;
use std::hash::Hash;

/// The one character every name the compiler mints starts with, whether local,
/// label, global, or synthesized struct or function. A Wado identifier holds no
/// dollar sign, so a minted name collides with nothing an author wrote and no
/// source can `break` to a synthesized label.
pub const INTERNAL_PREFIX: &str = "$";

/// Canonical method name of the synthesised `$call` impl on every
/// closure functor struct. Defined as a single constant so the
/// compiler-internal naming convention has one source of truth — the
/// closure planner, the translator, and DCE all reach for this
/// constant instead of writing the literal `"$call"` at each site.
///
/// Unlike stdlib items wired through [`crate::compiler_item`], the
/// `$call` symbol has no Wado-side declaration, so a `CompilerItem`
/// anchor would have nothing to bind to. A `const` is the right shape.
pub const CLOSURE_CALL_METHOD: &str = "$call";

/// Separator between a namespace-import alias and the imported member in the
/// canonical `ns$member` name a `ns::member` reference resolves to. `$` is not
/// a valid Wado identifier character, so the alias is a single `::`-free token
/// that flows through the resolution table's import tier and the per-name
/// import maps (`imported_globals`, the symbol-table imports) like a
/// `use { X as Y }` alias, scoped to the namespace's own module.
pub const NAMESPACE_MEMBER_SEP: char = '$';

/// Build the `ns$member` alias for a namespace-imported symbol. A namespace
/// that is itself a path, `ns::Interface`, joins on the same separator.
pub fn namespace_member_alias(namespace: &str, member: &str) -> String {
    format!(
        "{}{NAMESPACE_MEMBER_SEP}{member}",
        namespace.replace("::", &NAMESPACE_MEMBER_SEP.to_string())
    )
}

/// Split a `ns$member` alias into its namespace and member. The member is an
/// identifier, so the last separator splits them, a minted `$ns` included. A
/// minted name holds its one `$` first, so it splits off no namespace and is
/// no alias: `$a:x/trip::Inner` is a type of the interface minted `$a:x/trip`.
pub fn split_namespace_member(name: &str) -> Option<(&str, &str)> {
    name.rsplit_once(NAMESPACE_MEMBER_SEP)
        .filter(|(namespace, _)| !namespace.is_empty())
}

/// Read a `ns$member` alias back as the `ns::member` its author wrote, for a
/// diagnostic. Any other name is already its own spelling.
pub fn unalias_namespace_member(name: &str) -> String {
    match split_namespace_member(name) {
        Some((namespace, member)) => namespaced_name(namespace, member),
        None => name.to_string(),
    }
}

/// A member reached through a namespace import, as source writes it:
/// `ns::member`.
pub fn namespaced_name(namespace: &str, member: &str) -> String {
    format!("{namespace}::{member}")
}

/// Separator between a local item's declared name and its disambiguating
/// `AstId` in the mangled name a function-scoped `struct` / `type` declaration
/// (`Stmt::Item`) renders to. `@` is not a valid Wado identifier character.
///
/// Written only by [`mangle_local_item_name`] and read back nowhere. What
/// tells two sibling declarations apart is their [`crate::defs::DefId`]; this
/// only keeps the *rendering* injective, which every mangle owes.
pub const LOCAL_ITEM_ID_SEP: char = '@';

/// Build the mangled name for a local item declaration: the declared name plus
/// the `AstId`'s module-local index, so two sibling functions' same-named local
/// items render to two names rather than one.
/// Only `local` is encoded, never the `AstIdSpace` — that is a process-global
/// counter, and encoding it would make mangled WIR names non-deterministic.
pub fn mangle_local_item_name(name: &str, id: AstId) -> String {
    format!("{name}{LOCAL_ITEM_ID_SEP}{}", id.local())
}

/// The name of the synthesized deep-copy helper for a value type, identified
/// by its module-qualified structural mangle
/// (`TypeTable::mangle_type_arg_for_generic`). The mangle is a stable,
/// path-independent identity — unlike an intern-order `TypeId` index — so the
/// same helper carries the same name in the WIR dump, the codegen output, and
/// the wasm name section, and identical types that were interned more than
/// once collapse onto one helper.
pub fn value_copy_helper_name(mangled_type: &str) -> String {
    format!("$value_copy${mangled_type}")
}

/// The name of the shallow-copy sibling `value_copy_demote` synthesizes for a
/// deep value-copy helper: the deep helper's name with a `$shallow` suffix.
pub fn shallow_copy_helper_name(deep_name: &str) -> String {
    format!("{deep_name}$shallow")
}

/// The name of the function `cold_outline` moves a `cold_path()` region into:
/// the enclosing function's name with a `$cold` suffix, numbered when it opens
/// more than one region.
pub fn cold_region_helper_name(enclosing_name: &str, ordinal: u32) -> String {
    format!("{enclosing_name}$cold{ordinal}")
}

/// The name of a function's `unchecked` twin, which holds its body without
/// its `contract` clauses: the function's name with an `$unchecked` suffix.
pub fn unchecked_twin_name(name: &str) -> String {
    format!("{name}{UNCHECKED_TWIN_SUFFIX}")
}

/// The name a twin's function goes by, where `name` may name the twin.
pub fn checked_name(name: &str) -> &str {
    name.trim_suffix(UNCHECKED_TWIN_SUFFIX)
}

const UNCHECKED_TWIN_SUFFIX: &str = "$unchecked";

/// The name of the function reporting the failure of the `ordinal`th
/// `contract` clause of the function `function` identifies, shared by every
/// call checking it. It lives in the function's module, so the module is left
/// out of it.
pub fn contract_report_name(function: &FunctionId, ordinal: u32) -> String {
    match function {
        FunctionId::Free(free) => format!("{}$contract_report{ordinal}", free.name),
        FunctionId::Method(method) => format!("{method}$contract_report{ordinal}"),
    }
}

/// The `core:builtin` intrinsic heading each `contract` clause of a function,
/// `if $contract_clause() { … }`, which `lower::contract_checks` copies to
/// each call and then unwraps. No source spells it.
pub const CONTRACT_CLAUSE_INTRINSIC: &str = "$contract_clause";

/// The `core:builtin` intrinsic a failed `contract` clause reports its
/// position with, `$contract_site("module:line")`: `lower::contract_checks`
/// answers the call's position where it copies the clause to a call, and the
/// clause's own where a call reaches the function through a value.
pub const CONTRACT_SITE_INTRINSIC: &str = "$contract_site";

/// The `core:builtin` intrinsic marking a path rare: its branch is unlikely,
/// and it is cold code.
pub const COLD_PATH_INTRINSIC: &str = "cold_path";

/// The `core:builtin` intrinsic a profile writes where a branch is unlikely but
/// its path is not cold code.
pub const UNLIKELY_PATH_INTRINSIC: &str = "unlikely_path";

/// The `core:builtin` intrinsic a profile writes where a path is cold code but
/// its branch is not unlikely.
pub const COLD_CODE_INTRINSIC: &str = "cold_code";

/// The name of the hidden default method a trait declares to hold the
/// `contract` clauses of its method `method`, which every impl of the method
/// calls at its head.
pub fn contract_holder_name(method: &str) -> String {
    format!("{CONTRACT_HOLDER_PREFIX}{method}")
}

const CONTRACT_HOLDER_PREFIX: &str = "$contract$";

/// Whether `function_name` names a trait's `contract` clause holder.
pub fn is_contract_holder(function_name: &str) -> bool {
    function_name.contains(CONTRACT_HOLDER_PREFIX)
}

/// The function a failed `contract` clause names, from the name of the one
/// reifying it: the method a clause holder holds the clauses of.
pub fn contract_owner_name(function_name: &str) -> String {
    function_name.replacen(CONTRACT_HOLDER_PREFIX, "", 1)
}

/// The name of the function holding an effect operation's default
/// implementation — the body that runs when the operation is dispatched with
/// no handler installed. `$`-prefixed like the other synthesized helpers, so it
/// never collides with a source-level function: `core:log` declares both a
/// `Log::event` default and an `event` facade in one module.
pub fn effect_default_impl_name(interface: &str, op: &str) -> String {
    format!("{EFFECT_DEFAULT_PREFIX}{interface}${op}")
}

/// Prefix of [`effect_default_impl_name`].
const EFFECT_DEFAULT_PREFIX: &str = "$effect_default$";

/// Whether `name` is an [`effect_default_impl_name`].
#[must_use]
pub fn is_effect_default(name: &str) -> bool {
    name.starts_with(EFFECT_DEFAULT_PREFIX)
}

/// How a function name reads where a user sees it — `#function`, and the
/// call-site name a defaulted argument captures. A default implementation's
/// storage name is an internal detail, so it renders as the operation it
/// implements (`Log::event`); every other name passes through unchanged.
#[must_use]
pub fn display_function_name(name: &str) -> String {
    let Some(rest) = name.strip_prefix(EFFECT_DEFAULT_PREFIX) else {
        return name.to_string();
    };
    match rest.split_once('$') {
        Some((interface, op)) => format!("{interface}::{op}"),
        None => name.to_string(),
    }
}

/// The name a diagnostic shows for a function: a method as source spells it
/// (`String::grow`), from its structure; any other function by the name it was
/// declared under (`id<&P>`), the span already carrying its module. A clone's
/// suffix names no function an author wrote.
///
/// The function's own module is stripped as the exact prefix it is, since a
/// module path may hold any character. Only the paths inside type arguments
/// are found by search.
///
/// An effect default spells its interface with the same separator and is left
/// to [`display_function_name`].
#[must_use]
pub fn diagnostic_function_name(
    name: &str,
    module: &ModuleSource,
    method: Option<&LocalMethodName>,
) -> String {
    if is_effect_default(name) {
        return name.to_string();
    }
    if let Some(method) = method {
        let mut declared = method.clone();
        declared.method_name = as_declared(&method.method_name);
        return declared.to_display_name();
    }
    let local = name.strip_prefix(&format!("{module}/")).unwrap_or(name);
    as_declared(local)
}

/// The name of the module whose file is `path` inside the package whose
/// coordinate is `coordinate` (`acme:app@1.0.0+build.5`).
#[must_use]
pub fn packaged_module_name(coordinate: &str, path: &str) -> String {
    format!("{coordinate}/{path}")
}

/// How coverage and a profile name a function `name` declared under `owner`,
/// the type or trait its methods' block names.
#[must_use]
pub fn coverage_function_name(owner: Option<&str>, name: &str) -> String {
    match owner {
        Some(owner) => format!("{owner}::{name}"),
        None => name.to_string(),
    }
}

/// How coverage and a profile name a closure written in the function named
/// `outer`, at `line`.
#[must_use]
pub fn coverage_closure_name(outer: &str, line: usize) -> String {
    format!("{outer}::{{closure:{line}}}")
}

/// `name` without the version a [`packaged_module_name`]'s coordinate spells:
/// what one module of a package is named under each of its versions.
#[must_use]
pub fn unversioned_module_name(name: &str) -> String {
    let Some((head, path)) = name.split_once('/') else {
        return name.to_string();
    };
    match head.split_once('@') {
        Some((package, _version)) => format!("{package}/{path}"),
        None => name.to_string(),
    }
}

/// `name` with the module paths in its type arguments dropped and its clone
/// suffix cut. A synthesized name opens with [`INTERNAL_PREFIX`], which is its
/// own first character rather than a suffix marker; so may a type argument.
fn as_declared(name: &str) -> String {
    // What a path and a [`packaged_module_name`]'s semver coordinate spell.
    let is_module_path =
        |c: char| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | ':' | '-' | '@' | '+');
    let mut out = String::with_capacity(name.len());
    let mut depth = 0u32;
    for c in name.chars() {
        match c {
            '/' => {
                let path_start = out
                    .char_indices()
                    .rev()
                    .find(|&(_, c)| !is_module_path(c))
                    .map_or(0, |(i, c)| i + c.len_utf8());
                out.truncate(path_start);
                continue;
            }
            '<' | '(' | '[' => depth += 1,
            '>' if !out.ends_with('-') => depth -= 1,
            ')' | ']' => depth -= 1,
            '$' if depth == 0 && !out.is_empty() => break,
            _ => {}
        }
        out.push(c);
    }
    out
}

/// The name of a `param_spec` clone: the original's name plus the clone's
/// ordinal among that callee's specializations. Unique package-wide, since the
/// original's name already is.
pub fn param_spec_name(original: &str, ordinal: usize) -> String {
    format!("{original}$spec{ordinal}")
}

/// `{base}{marker}[a,b]`: a transformation's suffix, its arguments grouped so
/// that no identifier inside can be read as the next suffix.
fn grouped_suffix(base: &str, marker: &str, items: &[String]) -> String {
    assert!(!items.is_empty(), "a suffix groups one item at least");
    format!("{base}{INTERNAL_PREFIX}{marker}[{}]", items.join(","))
}

const DAE_MARKER: &str = "dae";

/// The name of the function derived from `root` that takes each `narrowed`
/// parameter's field where `root` took the struct around it, none of the
/// `dropped` ones, both in `root`'s declaration order, and returns as `ret`
/// says: `f$sroa[self.used]`, `g$sroa[&mut(o.inner.x),&p.y]$dae[q,return]`.
/// The name spells everything that makes the function one, so two routes to
/// one shape arrive at one name. `return` is a keyword, so no parameter is
/// named that, and `&mut` groups its path so that a parameter whose name
/// starts with `mut` reads as one.
#[must_use]
pub fn reshaped_name(
    root: &str,
    narrowed: &[NarrowedParam<'_>],
    dropped: &[&str],
    ret: ReturnShape,
) -> String {
    const RETURN: &str = "return";
    let mut sroa: Vec<String> = narrowed
        .iter()
        .map(|n| {
            let mut path = vec![n.param];
            path.extend(&n.fields);
            let path = path.join(".");
            match n.form {
                FieldForm::Value => path,
                FieldForm::Shared => format!("&{path}"),
                FieldForm::Mutable => format!("&mut({path})"),
            }
        })
        .collect();
    let mut dae: Vec<String> = dropped.iter().map(ToString::to_string).collect();
    match ret {
        ReturnShape::Kept => {}
        ReturnShape::Scalarized => sroa.push(RETURN.to_string()),
        ReturnShape::Dropped => dae.push(RETURN.to_string()),
    }
    let mut name = root.to_string();
    if !sroa.is_empty() {
        name = grouped_suffix(&name, "sroa", &sroa);
    }
    if !dae.is_empty() {
        name = grouped_suffix(&name, DAE_MARKER, &dae);
    }
    name
}

/// `name` without the parameters [`reshaped_name`] lists dropped, or `name` itself:
/// only a `$dae[…]` group ending the name is peeled.
#[must_use]
fn before_dae(name: &str) -> &str {
    let opener = format!("{INTERNAL_PREFIX}{DAE_MARKER}[");
    match name.rfind(&opener) {
        Some(at) if name.ends_with(']') && !name[at + opener.len()..].contains('[') => &name[..at],
        _ => name,
    }
}

/// The name a function gives up its own for once it is dead and another
/// stands in for it: `f$retired[17]`, unique by the serial it carries, the
/// function's store position.
#[must_use]
pub fn retired_name(name: &str, serial: usize) -> String {
    grouped_suffix(name, "retired", &[serial.to_string()])
}

/// A parameter as [`reshaped_name`] lists it: its name, and where another
/// parameter shares that name (`_`), its position as well: `_#1`.
#[must_use]
pub fn param_label(name: &str, shared_at: Option<usize>) -> String {
    match shared_at {
        Some(position) => format!("{name}#{position}"),
        None => name.to_string(),
    }
}

/// One parameter `sroa_param` narrowed to a field: the parameter, the fields
/// it was projected through, outermost first, and how it holds the last one.
pub struct NarrowedParam<'a> {
    pub param: &'a str,
    pub fields: Vec<&'a str>,
    pub form: FieldForm,
}

/// The WIR local holding one field of an aggregate the multi-value ABI took
/// apart, on either the result side or the parameter side.
// Opens with and separates on [`INTERNAL_PREFIX`], so no source local spells
// one and no two `(base, field_name)` pairs spell the same one.
#[must_use]
pub fn multi_value_split_local(base: &str, field_name: &str) -> String {
    format!("{INTERNAL_PREFIX}mv{INTERNAL_PREFIX}{base}{INTERNAL_PREFIX}{field_name}")
}

/// The `discriminant` of one instantiated generic variant, identified by the
/// instance's structural mangle. A free helper like the value bridges: the
/// method-name form cannot spell an instance.
pub fn variant_tag_helper_name(mangled_variant: &str) -> String {
    format!("$variant_tag${mangled_variant}")
}

/// The helper rebuilding a value of one type, holding an `Option<!>`, as the
/// type holding an `Option<T>` there, both identified by their structural
/// mangles.
pub fn widen_helper_name(mangled_from: &str, mangled_to: &str) -> String {
    format!("$widen${mangled_from}${mangled_to}")
}

/// The kinds of bridge a reflection marker calls: a helper synthesized per head
/// type (and member type), both by structural mangle, that `lower` rewrites
/// the marker's call to (WEP 2026-06-13 §2 and §3e, WEP 2026-01-10).
/// [`variant_tag_helper_name`] is not one.
#[derive(Clone, Copy)]
pub enum MarkerBridge {
    CaseExtract,
    CaseRef,
    CaseConstruct,
    FieldGet,
    FieldRef,
    HoleGet,
    HoleRef,
    TemplateSegment,
    HoleFmt,
}

impl MarkerBridge {
    pub const ALL: [Self; 9] = [
        Self::CaseExtract,
        Self::CaseRef,
        Self::CaseConstruct,
        Self::FieldGet,
        Self::FieldRef,
        Self::HoleGet,
        Self::HoleRef,
        Self::TemplateSegment,
        Self::HoleFmt,
    ];

    /// The `builtin::` marker that `lower` rewrites into a call of this bridge.
    #[must_use]
    pub const fn marker(self) -> &'static str {
        match self {
            Self::CaseExtract => "variant_case_extract",
            Self::CaseRef => "variant_case_ref",
            Self::CaseConstruct => "variant_case_construct",
            Self::FieldGet => "struct_field_get",
            Self::FieldRef => "struct_field_ref",
            Self::HoleGet => "hole_get",
            Self::HoleRef => "hole_ref",
            Self::TemplateSegment => "template_segment",
            Self::HoleFmt => "hole_fmt",
        }
    }

    /// Whether the bridge names a member type besides its head, by
    /// [`Self::pair_name`], rather than the template shape alone, by
    /// [`Self::shape_name`].
    #[must_use]
    pub const fn names_member(self) -> bool {
        !matches!(self, Self::TemplateSegment | Self::HoleFmt)
    }

    /// What every bridge of this kind starts with.
    #[must_use]
    pub const fn prefix(self) -> &'static str {
        match self {
            Self::CaseExtract => "$case_extract$",
            Self::CaseRef => "$case_ref$",
            Self::CaseConstruct => "$case_construct$",
            Self::FieldGet => "$field_get$",
            Self::FieldRef => "$field_ref$",
            Self::HoleGet => "$hole_get$",
            Self::HoleRef => "$hole_ref$",
            Self::TemplateSegment => "$template_segment$",
            Self::HoleFmt => "$hole_fmt$",
        }
    }

    /// The bridge for a head type and one member type, by structural mangle.
    #[must_use]
    pub fn pair_name(self, mangled_head: &str, mangled_member: &str) -> String {
        format!("{}{mangled_head}${mangled_member}", self.prefix())
    }

    /// The bridge for a template shape alone, by structural mangle.
    #[must_use]
    pub fn shape_name(self, mangled_shape: &str) -> String {
        format!("{}{mangled_shape}", self.prefix())
    }
}

/// Name of the synthesized function that drops the resources a value of a
/// self-holding type holds, by structural mangle of the type.
pub fn drop_glue_name(mangled_type: &str) -> String {
    format!("{DROP_GLUE_PREFIX}{mangled_type}")
}

/// What every [`drop_glue_name`] starts with.
pub const DROP_GLUE_PREFIX: &str = "$drop$";

/// Whether `name` is a per-type bridge — a helper synthesis mints for a *type*
/// rather than for a call site: the value-copy pair, the reflect accessors
/// (each [`MarkerBridge`]), an effect operation's
/// default. No TIR body names one: lowering rewrites a `builtin::…` call to it,
/// or the dispatch reaches it, so a reachability walk over TIR bodies must root
/// them all.
///
/// Every such helper spells the mangle of the type it belongs to after its
/// kind, so a second [`INTERNAL_PREFIX`] is what tells one from the plainly
/// synthesized functions that carry no type (`$initialize_module`, `$test_0`,
/// `$cm_export__run`). A [`global_init_function`] spells a global there, not a
/// type, and its global's uses reach it.
#[must_use]
pub fn is_type_bridge(name: &str) -> bool {
    global_init_target(name).is_none()
        && name
            .strip_prefix(INTERNAL_PREFIX)
            .is_some_and(|rest| rest.contains(INTERNAL_PREFIX))
}

/// A minted name's digest part: the first 8 bytes of `digest`, in hex.
fn short_digest(digest: &[u8]) -> String {
    digest[..8]
        .iter()
        .fold(String::with_capacity(16), |mut hex, byte| {
            write!(hex, "{byte:02x}").expect("a String accepts every write");
            hex
        })
}

/// The mangled name of the template shape whose hash is `digest`.
pub fn template_shape_name(digest: &[u8; 32]) -> String {
    format!("$tmpl${}", short_digest(digest))
}

/// Field name of the discriminant slot on a variant's base struct (the tag
/// every case subtype inherits). Single source of truth for the WIR variant
/// GC representation: the builder (`wir_build/types.rs`, `pattern_match.rs`,
/// `translate.rs`), the multi-value SROA (`sroa_variant_return.rs`), the codegen
/// field map (`codegen/emit.rs`), and the `nullable_ref` lowering all key on
/// this exact name, so a literal drift would produce invalid Wasm. Builder-side
/// call sites should adopt this constant (and [`variant_payload_field`] /
/// [`is_variant_payload_field`]) so the convention cannot silently diverge.
pub const VARIANT_DISCRIMINANT_FIELD: &str = "discriminant";

/// Field name of the `index`-th payload slot on a variant case subtype
/// (`payload_0`, `payload_1`, ...). Paired with [`VARIANT_DISCRIMINANT_FIELD`]
/// as the canonical variant-field naming; see that constant's note.
pub fn variant_payload_field(index: usize) -> String {
    format!("payload_{index}")
}

/// Whether `field_name` is a variant case payload slot (see
/// [`variant_payload_field`]). Matches the `payload_` prefix; the multi-value
/// SROA's `case{N}_payload_{M}` slots carry a `case` prefix and so are
/// deliberately not matched.
pub fn is_variant_payload_field(field_name: &str) -> bool {
    field_name.starts_with("payload_")
}

/// Whether the CM interface or function `fq` is WASI's, which the host answers,
/// rather than a guest's.
pub fn is_wasi_fq(fq: &str) -> bool {
    fq.starts_with("wasi:")
}

/// The core function of a world export `name`'s binding.
pub fn cm_export_func_name(name: &str) -> String {
    format!("{INTERNAL_PREFIX}cm_export__{name}")
}

/// The core function of a sync lift's `post-return` for the export `name`.
pub fn cm_post_return_func_name(name: &str) -> String {
    format!("{INTERNAL_PREFIX}cm_post_return__{name}")
}

/// The copy of the `export async fn` `name` its binding calls.
pub fn cm_task_entry_func_name(name: &str) -> String {
    format!("{INTERNAL_PREFIX}cm_task_entry__{name}")
}

/// The core export the host calls a closure back through, for the shape
/// `name` that [`callback_export_name`] gives.
pub fn cm_callback_func_name(name: &str) -> String {
    format!("{INTERNAL_PREFIX}cm_callback__{name}")
}

/// An interface's basename: the last label of its interface name, without the
/// version, in `PascalCase` (`a:x/trip@0.1.0` is `Trip`).
pub fn interface_basename(interface: &ast::CmImport) -> String {
    interface.interface.to_upper_camel_case()
}

/// The name a component's interface is declared under where its basename is
/// not its own: its interface name, which no Wado identifier can spell.
pub fn unnamed_interface_name(interface: &ast::CmImport) -> String {
    format!("{INTERNAL_PREFIX}{}", interface.interface_path())
}

// A name a dependency component exports is its own: two dependencies may each
// export a world function, or an interface, of one name. The program imports
// each under a name tagged with its component, and composition wires that name
// to the component's export. The names are CM names, so they carry no `$`.

/// The tag of the dependency whose canonical namespace is `component`: a
/// digest, so it depends on no order the dependencies are met in.
fn cm_dependency_tag(component: &str) -> String {
    format!(
        "dependency{}",
        short_digest(&Sha256::digest(component.as_bytes()))
    )
}

/// The label the program imports the world function `function` of the
/// dependency `component` under.
pub fn cm_world_import_label(component: &str, function: &str) -> String {
    format!("{}-{function}", cm_dependency_tag(component))
}

/// The interface the program imports the interface `interface` the dependency
/// `component` exports as.
pub fn cm_dependency_interface(interface: &ast::CmImport, component: &str) -> ast::CmImport {
    ast::CmImport {
        package: format!("{}-{}", interface.package, cm_dependency_tag(component)),
        ..interface.clone()
    }
}

// A CM import's names are minted from its CM identifier
// (`wasi:cli/stdout@0.3.0#write-via-stream`, or a world function's label),
// which is what identifies it: a Wado spelling such as `Stdout::write_via_stream`
// is shared by same-named interfaces.

/// The module the main module imports its web imports from: the functions
/// whose signature holds an extern handle, which are no component imports. A
/// trapping module the component embeds satisfies it, and a browser's loader
/// replaces that one (WEP: Extern Handles).
pub const EXTERN_IMPORT_MODULE: &str = "extern";

/// What the guest asks the host of its own objects, which carry neither a
/// class nor a backing value. Each is an import of `EXTERN_IMPORT_MODULE`
/// named by `cm_import_alias`, beside the web imports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ExternHostQuery {
    /// `class-of(object) -> i32`: the class of the nearest ancestor of the
    /// object's interface that declares one.
    ClassOf,
    /// `same(a, b) -> i32`: whether two objects are one.
    Same,
    /// `promise-wait(promise, buffer)`: suspends until the promise an `async`
    /// web import answered settles, then has the result written at `buffer`.
    PromiseWait,
}

impl ExternHostQuery {
    pub fn import_name(self) -> String {
        cm_import_alias(match self {
            Self::ClassOf => "class-of",
            Self::Same => "same",
            Self::PromiseWait => "promise-wait",
        })
    }

    /// How many objects it takes, ahead of any address.
    pub fn objects(self) -> usize {
        match self {
            Self::ClassOf | Self::PromiseWait => 1,
            Self::Same => 2,
        }
    }

    /// Whether it takes an address in linear memory after its objects.
    pub fn takes_address(self) -> bool {
        match self {
            Self::ClassOf | Self::Same => false,
            Self::PromiseWait => true,
        }
    }

    /// Whether it answers an `i32`, rather than nothing.
    pub fn answers(self) -> bool {
        match self {
            Self::ClassOf | Self::Same => true,
            Self::PromiseWait => false,
        }
    }
}

/// How the host reaches the slots extern handles are in while they cross in
/// linear memory. Each is a core export of the main module.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ExternSlotAccess {
    /// `put(object) -> i32`: the slot the guest puts the object in.
    Put,
    /// `get(slot) -> object`: the object in a slot, which stays there.
    Get,
}

impl ExternSlotAccess {
    pub fn export_name(self) -> String {
        let access = match self {
            Self::Put => "put",
            Self::Get => "get",
        };
        format!("{INTERNAL_PREFIX}cm_extern_slot_{access}")
    }
}

/// The core function a CM import lowers to.
pub fn cm_import_alias(cm_identifier: &str) -> String {
    format!("{INTERNAL_PREFIX}{cm_identifier}")
}

/// The binding of a CM import, which its call sites call.
pub fn cm_binding_func_name(cm_identifier: &str) -> String {
    format!("{INTERNAL_PREFIX}cm_binding__{cm_identifier}")
}

/// The lift of an async CM import, which `AsyncCall<T>` points to.
pub fn cm_lift_func_name(cm_identifier: &str) -> String {
    format!("{INTERNAL_PREFIX}cm_lift__{cm_identifier}")
}

/// The function freeing what an async CM import's arguments were lowered into,
/// which `ArgBuffer` points to.
pub fn cm_release_args_func_name(cm_identifier: &str) -> String {
    format!("{INTERNAL_PREFIX}cm_release_args__{cm_identifier}")
}

/// The function packaging a handler's result of an async CM import as the
/// `AsyncCall<T>` its call site reads.
pub fn cm_wrap_async_func_name(cm_identifier: &str) -> String {
    format!("{INTERNAL_PREFIX}cm_wrap_async__{cm_identifier}")
}

/// A helper synthesized per payload type of a CM future or stream, named by
/// [`Self::func_name`] after the payload's mangle (or, for a record stream,
/// the WASI record's name).
#[derive(Clone, Copy)]
pub enum CmChannelHelper {
    /// Writes a future.
    FutureWrite,
    /// Reads a future.
    FutureRead,
    /// Starts a read of a future.
    FutureBeginRead,
    /// Lifts what a read copied from a future.
    FutureLift,
    /// Writes a stream.
    StreamWrite,
    /// Reads one value from a stream.
    StreamReadValue,
    /// Reads a stream of a WASI record.
    StreamRead,
    /// Lifts what a read copied from a stream.
    StreamLiftValue,
    /// Lifts what a read copied from a stream of a WASI record.
    StreamLift,
}

impl CmChannelHelper {
    #[must_use]
    pub fn func_name(self, payload: &str) -> String {
        let what = match self {
            Self::FutureWrite => "future_write",
            Self::FutureRead => "future_read",
            Self::FutureBeginRead => "future_begin_read",
            Self::FutureLift => "future_lift",
            Self::StreamWrite => "stream_write",
            Self::StreamReadValue => "stream_read_val",
            Self::StreamRead => "stream_read",
            Self::StreamLiftValue => "stream_lift_val",
            Self::StreamLift => "stream_lift",
        };
        format!("{INTERNAL_PREFIX}cm_{what}_{payload}")
    }
}

/// What a callback export's name calls an extern handle argument.
pub const CALLBACK_HANDLE_WORD: &str = "handle";

/// The shape of the export the host calls a closure back through, given a word
/// per argument: its Wado primitive, or [`CALLBACK_HANDLE_WORD`]
/// (`call-handle`).
pub fn callback_export_name<'a>(params: impl IntoIterator<Item = &'a str>) -> String {
    std::iter::once("call")
        .chain(params)
        .collect::<Vec<_>>()
        .join("-")
}

/// The Component Model name of the Wado identifier `name`: the label a
/// function, parameter, field, case or interface crosses the boundary under.
///
/// The CM spec fixes what a label is, and nothing about how a source language
/// arrives at one (`Explainer.md` § "Import and Export Definitions"):
///
/// ```text
/// label          ::= first-fragment ( '-' fragment )*
/// first-fragment ::= [a-z] [0-9a-z]* | [A-Z] [0-9A-Z]*
/// fragment       ::= [0-9a-z]+ | [0-9A-Z]+
/// ```
///
/// Only the first fragment starts with a letter, so `a-1` and `x-2` are labels.
/// Names in one scope must also be strongly unique: equal once hyphens are
/// dropped and letters lowercased, as `a-b` and `ab` are, they are one name
/// (§ "Name Uniqueness"). This maps `snake_case` / `PascalCase` / `camelCase`
/// to that casing (`my-api`, `http-server`, `error-code`) and checks neither
/// rule: two identifiers that become one name, such as `a_b` and `aB`, make a
/// component the Wasm validator rejects.
///
/// Public so out-of-crate boundary code (e.g. the CLI host materializing a
/// generator's options from canonical CBOR) maps raw Wado identifiers to the CM
/// names a component exposes, instead of re-deriving the format.
pub fn to_cm_name(name: &str) -> String {
    name.to_kebab_case()
}

/// The CM export name of a test function (`$test_0_my_name` as `test-0-my-name`).
/// The elaborator spells each non-alphanumeric character of the test's string as
/// `_`, so a name may hold `__` or end in `_`, which the CM name collapses.
pub fn test_export_name(function_name: &str) -> String {
    to_cm_name(function_name.trim_start_matches(INTERNAL_PREFIX))
}

/// What the Component Model compares the CM name `cm_name` by when it requires
/// names in one scope to be strongly unique: `a-b`, `ab` and `A-B` share one.
pub fn cm_name_key(cm_name: &str) -> String {
    cm_name.replace('-', "").to_ascii_lowercase()
}

/// Prefix the compiler stamps onto every synthesised closure-functor
/// struct (`$Closure_0`, `$Closure_1`, …). Like
/// [`CLOSURE_CALL_METHOD`], this is purely a compiler-internal
/// convention — there is no Wado-side declaration to anchor it to.
pub const CLOSURE_STRUCT_PREFIX: &str = "$Closure_";

/// The WIR struct an affine resource is: the object holding the host's handle,
/// or the guest value backing it (WEP: Guest-Backed Resources). One struct
/// holds either, so a store through `&mut R` replaces one with the other. Not a
/// cell, which the optimizer scalarizes as a value with no identity.
pub const RESOURCE_OBJECT_STRUCT: &str = "$Resource";
/// The host's handle, 0 where there is none: index 0 of a handle table is
/// reserved.
pub const RESOURCE_HANDLE_FIELD: &str = "value";
/// The guest value backing the resource, null for the host's object.
pub const RESOURCE_BACKING_FIELD: &str = "backing";

/// The WIR struct behind an extern handle a guest value backs: the handle's
/// class and the value. A host object is never one, which is how the two are
/// told apart (WEP: Extern Handles).
pub const BACKED_HANDLE_STRUCT: &str = "$BackedHandle";
pub const BACKED_HANDLE_CLASS_FIELD: &str = "class";
pub const BACKED_HANDLE_VALUE_FIELD: &str = "value";

/// The object of one field, `value`, that a reference to a primitive, an enum,
/// a variant, a function or another reference points at.
pub const CELL_STRUCT: &str = "$Cell";

/// The one field of a [`CELL_STRUCT`].
pub const CELL_FIELD: &str = "value";

/// How a cell's name spells an extern handle payload: every one is an
/// `externref`, so a reference to a resource is one to each it extends.
pub const CELL_EXTERN_PAYLOAD: &str = "extern";

/// The WIR struct a cell holding the payload spelled `payload` lowers to.
#[must_use]
pub fn cell_struct_name(payload: &str) -> String {
    mangle_generic_name(CELL_STRUCT, &[payload.to_string()])
}

/// The name closure `functor_id`'s functor struct is interned under.
#[must_use]
pub fn closure_functor_struct_name(functor_id: u32) -> String {
    format!("{CLOSURE_STRUCT_PREFIX}{functor_id}")
}

/// The functor struct closure `functor_id` lowers to.
#[must_use]
fn closure_functor_type(module: &ModuleSource, functor_id: u32) -> FqTypeName {
    FqTypeName::shape(module, &closure_functor_struct_name(functor_id))
}

/// The `$call` triple codegen re-mangles a functor's method from.
#[must_use]
pub fn closure_call_method_info(module: &ModuleSource, functor_id: u32) -> LocalMethodName {
    LocalMethodName::new(
        closure_functor_type(module, functor_id),
        None,
        CLOSURE_CALL_METHOD.to_string(),
    )
}

/// The name of the `global_id`-th closure's function-table wrapper forwarding to
/// its `$call`.
#[must_use]
pub fn closure_call_wrapper_name(module: &ModuleSource, global_id: usize) -> String {
    format!("closure/{module}/{INTERNAL_PREFIX}closure_wrapper_{global_id}")
}

/// The name of the `global_id`-th closure's function-table wrapper forwarding to
/// its `^Inspect` impl.
#[must_use]
pub fn closure_inspect_wrapper_name(module: &ModuleSource, global_id: usize) -> String {
    format!("closure/{module}/{INTERNAL_PREFIX}closure_inspect_wrapper_{global_id}")
}

/// The mangled name of closure functor `functor_id`'s `$call`.
#[must_use]
pub fn closure_call_name(module: &ModuleSource, functor_id: u32) -> String {
    MethodName::format_local(
        &closure_functor_type(module, functor_id),
        None,
        CLOSURE_CALL_METHOD,
    )
}

/// Field name of the `index`-th environment slot on a closure functor
/// (`$capture_0`, `$capture_1`, …).
#[must_use]
pub fn closure_capture_field(index: u32) -> String {
    format!("{INTERNAL_PREFIX}capture_{index}")
}

/// The name a closure's `$call` gives its `index`-th parameter where the source
/// discards it with `_`: each `$call` parameter needs a name of its own, since
/// the function-table wrapper finds it among the closure's by name.
#[must_use]
pub fn discarded_param_name(index: usize) -> String {
    minted_name("discarded", index)
}

/// Display name of a capture read through the `&mut` box a mutating closure
/// holds, where the slot rather than the binding names the read.
#[must_use]
pub fn deref_capture_name(index: u32) -> String {
    format!("{INTERNAL_PREFIX}deref_cap_{index}")
}

/// The label on a C-style `for` loop's body, which `continue` breaks out of.
#[must_use]
pub fn for_body_label(serial: u32) -> String {
    format!("{INTERNAL_PREFIX}for_{serial}_body")
}

/// Whether `label` is one [`for_body_label`] minted: what runs after it in the
/// loop is the header's update, which acts on the next iteration's bindings.
#[must_use]
pub fn is_for_body_label(label: &str) -> bool {
    label
        .strip_prefix(INTERNAL_PREFIX)
        .and_then(|rest| rest.strip_prefix("for_"))
        .and_then(|rest| rest.strip_suffix("_body"))
        .is_some_and(|serial| serial.parse::<u32>().is_ok())
}

/// The reference proxy an owning frame binds for a binding a closure captures
/// by reference.
#[must_use]
pub fn capture_ref_name(var_name: &str) -> String {
    format!("{INTERNAL_PREFIX}ref_{var_name}")
}

/// The local a constant pattern compared by `Eq` holds its scrutinee in.
#[must_use]
pub fn constant_pattern_local_name() -> String {
    format!("{INTERNAL_PREFIX}constant")
}

/// The local a range pattern compared by tests holds its scrutinee in: a
/// float range, or one with a constant bound.
#[must_use]
pub fn range_local_name() -> String {
    format!("{INTERNAL_PREFIX}range")
}

/// Local holding the `f32` a half scrutinee of a float pattern widens to, so
/// its comparisons widen it once.
#[must_use]
pub fn widened_half_local_name() -> String {
    format!("{INTERNAL_PREFIX}range_f32")
}

/// Local holding one flattened field of a variant-return slot. `slot_local`
/// trails verbatim, so a source name and a minted one cannot collide.
#[must_use]
pub fn variant_return_field_local(field_name: &str, slot_local: &str) -> String {
    format!("{INTERNAL_PREFIX}vr_{field_name}_{slot_local}")
}

/// A local or label a construct mints for itself: `what` says which construct,
/// `serial` makes it unique within the body.
#[must_use]
pub fn minted_name(what: &str, serial: impl fmt::Display) -> String {
    format!("{INTERNAL_PREFIX}{what}_{serial}")
}

/// `what` narrowed by `sub` (a field name, say), for [`minted_name`].
#[must_use]
pub fn minted_what(what: &str, sub: impl fmt::Display) -> String {
    format!("{what}_{sub}")
}

/// The name a local that has none of its own goes by: its index, which the
/// local holds alone.
#[must_use]
pub fn indexed_local_name(index: impl fmt::Display) -> String {
    minted_name("local", index)
}

/// The suffix a callee specialised over closure functor structs takes: each
/// functor's mangled name after a `$`.
#[must_use]
pub fn functor_specialization_suffix(functors: impl IntoIterator<Item = String>) -> String {
    functors
        .into_iter()
        .fold(String::new(), |mut suffix, functor| {
            suffix.push_str(INTERNAL_PREFIX);
            suffix.push_str(&functor);
            suffix
        })
}

/// The path of a module the compiler writes for itself in directory `dir`,
/// which no source file can share.
#[must_use]
pub fn synthesized_module_path(dir: &str, stem: &str) -> String {
    let file = format!("{INTERNAL_PREFIX}{stem}.wado");
    if dir.is_empty() {
        file
    } else {
        format!("{dir}/{file}")
    }
}

/// The name of an anonymous struct, from its fields each rendered `name:type`.
#[must_use]
pub fn anonymous_struct_name(fields: &[String]) -> String {
    format!("{INTERNAL_PREFIX}anon_{{{}}}", fields.join(","))
}

/// The `index`th parameter of a synthesized function, which no source names.
#[must_use]
pub fn positional_param_name(index: impl fmt::Display) -> String {
    minted_name("p", index)
}

/// The local a power-assert binds a captured operand to.
#[must_use]
pub fn assert_capture_name(serial: u32) -> String {
    minted_name("v", serial)
}

/// The flag recording whether a conditional power-assert capture ran.
#[must_use]
pub fn assert_seen_name(capture: &str) -> String {
    format!("{capture}_seen")
}

/// The cold-branch local holding a conditional power-assert capture's text.
#[must_use]
pub fn assert_text_name(capture: &str) -> String {
    format!("{capture}_text")
}

/// What every LICM hoist is minted under, narrowed by [`minted_what`].
pub const LICM_HOIST: &str = "licm";

/// Whether `name` is one minted under [`LICM_HOIST`]: what tells a later LICM
/// session the hoists an earlier one left.
#[must_use]
pub fn is_licm_hoist(name: &str) -> bool {
    is_minted(name, LICM_HOIST)
}

/// Whether `name` is one [`minted_name`] gave under `what`.
fn is_minted(name: &str, what: &str) -> bool {
    name.strip_prefix(INTERNAL_PREFIX)
        .and_then(|rest| rest.strip_prefix(what))
        .is_some_and(|rest| rest.starts_with('_'))
}

/// What a contract check mints the local it binds an argument to under.
pub const CONTRACT_ARG: &str = "contract_arg";

/// Whether `name` is one minted under [`CONTRACT_ARG`].
#[must_use]
pub fn is_contract_arg(name: &str) -> bool {
    is_minted(name, CONTRACT_ARG)
}

/// The local a derived `Eq` binds `side`'s payload of variant case `case` to.
#[must_use]
pub fn eq_payload_local(side: &str, case: &str, index: u32) -> String {
    format!("{INTERNAL_PREFIX}eq_{side}_{case}_{index}")
}

/// What a copy of the local named `name` is minted under, for [`minted_name`].
#[must_use]
pub fn copied_local_what(name: &str) -> &str {
    name.trim_prefix(INTERNAL_PREFIX)
}

/// `name` told apart by `serial` from a same-spelled name already taken.
#[must_use]
pub fn serial_suffixed(name: &str, serial: u32) -> String {
    format!("{name}_{serial}")
}

/// The label of a block fusing labeled block `label` with the branch on its
/// value.
#[must_use]
pub fn fused_block_label(label: &str) -> String {
    format!("{INTERNAL_PREFIX}fused_{label}")
}

/// The label of a `match` that labeled block `label`'s exits are threaded into.
#[must_use]
pub fn threaded_block_label(label: &str) -> String {
    format!("{INTERNAL_PREFIX}thread_{label}")
}

/// The label of the block holding an inlined body of `callee`.
#[must_use]
pub fn inline_block_label(callee: &str, serial: u32) -> String {
    let callee: String = callee
        .chars()
        .map(|c| if is_ident_continue(c) { c } else { '_' })
        .collect();
    format!("{INTERNAL_PREFIX}inline_{callee}_{serial}")
}

/// The `ordinal`th temporary the splice labelled `label` binds for a `&mut`
/// argument, unique because the label is.
pub fn inline_temp_name(label: &str, ordinal: usize) -> String {
    let splice = label
        .strip_prefix(INTERNAL_PREFIX)
        .expect("an inline label is minted");
    minted_name(&minted_what("temp", splice), ordinal)
}

/// What the block binding a template's holes before its tag's call is minted
/// under.
const TEMPLATE_BLOCK: &str = "template";

/// The label of the block binding a template's holes before its tag's call.
#[must_use]
pub fn template_block_label(serial: u32) -> String {
    minted_name(TEMPLATE_BLOCK, serial)
}

/// The local binding hole `k` of the template [`template_block_label`] labels
/// with `serial`.
#[must_use]
pub fn template_hole_local(serial: u32, k: usize) -> String {
    format!("{INTERNAL_PREFIX}hole_{serial}_{k}")
}

/// Per-module initializer function the lowering phase synthesises to run a
/// module's global initializers. The optimizer's liveness / const-object
/// passes treat it as a root, so producer and consumers share this name.
pub const MODULE_INIT_FUNCTION: &str = "$initialize_module";

/// The function holding one global's initializer. Lowering splices these into
/// [`MODULE_INIT_FUNCTION`] and drops them.
#[must_use]
pub fn global_init_function(global: &str) -> String {
    format!("{INTERNAL_PREFIX}init{INTERNAL_PREFIX}{global}")
}

/// The global a [`global_init_function`] initializes, or `None` for any other
/// name.
#[must_use]
pub fn global_init_target(name: &str) -> Option<&str> {
    name.strip_prefix(INTERNAL_PREFIX)?
        .strip_prefix("init")?
        .strip_prefix(INTERNAL_PREFIX)
}

/// Aggregate initializer that calls every module's [`MODULE_INIT_FUNCTION`].
/// Shares the [`MODULE_INIT_FUNCTION`] prefix, so a `starts_with`
/// over the latter still covers both.
pub const MODULES_INIT_FUNCTION: &str = "$initialize_modules";

/// The global [`MODULES_INIT_FUNCTION`] sets once, so a second entry into the
/// component re-runs no initializer.
pub const MODULES_INIT_FLAG: &str = "$modules_initialized";

/// The namespace a Kiln generator's request adapter imports `core:kiln` under,
/// so what it names cannot meet a name the generator declares or imports.
pub const KILN_ADAPTER_NAMESPACE: &str = "$kiln";

/// `item` of `core:kiln`, as the request adapter spells it: the alias the
/// namespace import registers, which a type name resolves by as any name does.
#[must_use]
pub fn kiln_adapter_path(item: &str) -> String {
    namespace_member_alias(KILN_ADAPTER_NAMESPACE, item)
}

/// Prefix the const-object globalization pass stamps on the globals it hoists
/// constant aggregates into (`$const_obj_0`, …). It both mints and rescans
/// these names, so the prefix lives here rather than as a repeated literal.
pub const CONST_OBJ_GLOBAL_PREFIX: &str = "$const_obj_";

/// Maximum UTF-8 byte length for an `InlineRef`-hoisted global (see
/// [`crate::nir::NirGlobal::prefer_fixed_string_repr`]) to override the
/// package-wide `string_inline_max_bytes` threshold and materialize as a
/// constant `array.new_fixed<u8>`. Bounded so a large hoisted literal still
/// falls back to compact `array.new_data` instead of bloating `-Os` builds.
pub const INLINE_REF_EAGER_MAX_BYTES: usize = 64;

/// An IDL identifier as the `snake_case` name Wado declares it under: a keyword
/// the parser does not take as a name (`match`, `resume`) gets a trailing `_`.
#[must_use]
pub fn wado_identifier(idl_name: &str) -> String {
    let snake = idl_name.to_snake_case();
    let keyword = KEYWORDS
        .iter()
        .chain(CONTEXTUAL_KEYWORDS)
        .any(|(keyword, _)| *keyword == snake);
    if keyword && !NAME_KEYWORDS.contains(&snake.as_str()) {
        format!("{snake}_")
    } else {
        snake
    }
}

/// The symbol name of a module-level global var: `global:{module_source}::{name}`.
/// Every module is qualified the same way — there is no entry-module special
/// case, so the declaration and every read/write produce identical names by
/// construction and can never diverge.
#[must_use]
pub fn global_name(module_source: &ModuleSource, name: impl fmt::Display) -> String {
    format!("global:{module_source}::{name}")
}

/// The global holding the one shared object of a payload-less variant case:
/// `variant` is the variant's WIR type name, `case` the case's discriminant.
#[must_use]
pub fn unit_case_global_name(variant: &str, case: i32) -> String {
    global_name(
        &ModuleSource::builtin(),
        format!("{INTERNAL_PREFIX}unit_case{INTERNAL_PREFIX}{variant}{INTERNAL_PREFIX}{case}"),
    )
}

/// Build a local method name from struct name and method name.
///
/// Examples:
/// - `mangle_local_method("Point", "sum")` → `"Point::sum"`
pub fn mangle_local_method(struct_name: &str, method_name: &str) -> String {
    format!("{struct_name}::{method_name}")
}

/// The owner and member of a [`mangle_local_method`] name; `None` for a bare one.
#[must_use]
pub fn split_local_method(name: &str) -> Option<(&str, &str)> {
    name.rsplit_once("::")
}

/// Build a local method name with trait from struct name, trait name, and method name.
///
/// Examples:
/// - `mangle_local_trait_method("Point", "Display", "fmt")` → `"Point^Display::fmt"`
pub fn mangle_local_trait_method(struct_name: &str, trait_name: &str, method_name: &str) -> String {
    format!("{struct_name}^{trait_name}::{method_name}")
}

/// The namespace an `E::op()` call is rendered in, and the dispatch triple of
/// `E` is named after: the interface qualified by its declaring module, as two
/// modules may each declare an interface of one name.
///
/// Examples:
/// - `effect_namespace(./a.wado, "Base")` → `"./a.wado/Base"`
pub fn effect_namespace(module: &ModuleSource, interface: &str) -> String {
    format!("{module}/{interface}")
}

/// Build the per-instantiation effect-dispatch struct name.
///
/// `label` is the dispatch instantiation label produced by the
/// effect-dispatch synthesis (`./c.wado/Counter`, `./io.wado/Stream<u8>`, …).
///
/// Examples:
/// - `dispatch_struct_name("./c.wado/Counter")` → `"$Dispatch_./c.wado/Counter"`
pub fn dispatch_struct_name(label: &str) -> String {
    format!("$Dispatch_{label}")
}

/// Build the per-instantiation effect-dispatch global name.
///
/// Examples:
/// - `dispatch_global_name("./c.wado/Counter")` → `"$effect_./c.wado/Counter"`
pub fn dispatch_global_name(label: &str) -> String {
    format!("$effect_{label}")
}

/// Build the per-operation effect-dispatch wrapper function name.
///
/// Examples:
/// - `dispatch_wrapper_name("./c.wado/Counter", "next")` →
///   `"$effect_dispatch__./c.wado/Counter__next"`
pub fn dispatch_wrapper_name(label: &str, op_name: &str) -> String {
    format!("$effect_dispatch__{label}__{op_name}")
}

/// Build the dispatch struct's per-operation field name.
///
/// Examples:
/// - `dispatch_field_name("next")` → `"op_next"`
/// - `dispatch_field_name("read")` → `"op_read"`
pub fn dispatch_field_name(op_name: &str) -> String {
    format!("op_{op_name}")
}

/// Convert a `test "name"` string into the snake-case segment of the internal
/// test function name: `"café résumé"` → `"caf__r_sum_"`. Only ASCII
/// alphanumerics survive — Unicode-aware `is_alphanumeric` would let multibyte
/// letters through and produce an extern name Wasm validation rejects. The
/// lossless name is kept separately in the test-name custom section.
fn test_name_to_snake(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect::<String>()
        .to_lowercase()
}

/// What every test function's name opens with; what follows encodes the test's
/// attributes.
const TEST_PREFIX: &str = "$test";

/// Build a test block's exported name: `$test_{index}[_{snake}]`, with the
/// prefix encoding attributes — `$test_trap_…` for `#[expect_trap]`,
/// `$test_todo_…` for `#[TODO]`, `$test_tm{ms}_…` for `#[timeout_ms]`, and
/// combinations such as `$test_trap_tm{ms}_…`. Both the annotate walk and reify
/// call here, so the two cannot drift.
pub fn test_function_name(meta: &TestMetadata, test_index: usize, name: Option<&str>) -> String {
    let prefix = match (meta.is_todo, meta.expect_trap, meta.timeout_ms) {
        (true, _, Some(ms)) => format!("{TEST_PREFIX}_todo_tm{ms}"),
        (true, _, None) => format!("{TEST_PREFIX}_todo"),
        (_, true, Some(ms)) => format!("{TEST_PREFIX}_trap_tm{ms}"),
        (_, true, None) => format!("{TEST_PREFIX}_trap"),
        (_, _, Some(ms)) => format!("{TEST_PREFIX}_tm{ms}"),
        (_, _, None) => TEST_PREFIX.to_string(),
    };
    match name {
        Some(name) => format!("{prefix}_{test_index}_{}", test_name_to_snake(name)),
        None => format!("{prefix}_{test_index}"),
    }
}

/// Whether `name` is what [`test_function_name`] built, whatever attributes its
/// prefix encodes.
#[must_use]
pub fn is_test_function(name: &str) -> bool {
    name.starts_with(TEST_PREFIX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::AstIdSpace;
    use crate::module_source::ModuleSourceInterner;

    #[test]
    fn cm_names() {
        assert_eq!(to_cm_name("distance"), "distance");
        assert_eq!(to_cm_name("MyApi"), "my-api");
        assert_eq!(to_cm_name("set_level"), "set-level");
        assert_eq!(to_cm_name("HTTPServer"), "http-server");
        assert_eq!(to_cm_name("parse2html"), "parse2html");
        assert_eq!(to_cm_name("a_1"), "a-1");
    }

    #[test]
    fn before_dae_peels_only_the_dropped_parameters() {
        let x = NarrowedParam {
            param: "self",
            fields: vec!["x"],
            form: FieldForm::Value,
        };
        assert_eq!(
            before_dae(&reshaped_name(
                "$call",
                &[],
                &["self", "x"],
                ReturnShape::Kept
            )),
            "$call"
        );
        assert_eq!(before_dae("$call"), "$call");
        assert_eq!(
            before_dae(&reshaped_name("$call", &[x], &["y"], ReturnShape::Kept)),
            "$call$sroa[self.x]"
        );
    }

    #[test]
    fn diagnostic_name_drops_the_module_and_suffixes_but_keeps_a_minted_name() {
        let mut interner = ModuleSourceInterner::new();
        let entry = interner.entry_point("main.wado");
        let named = |module: &ModuleSource, name: &str| {
            let mangled = FreeFunctionName::new(module.clone(), name.to_string()).to_string();
            diagnostic_function_name(&mangled, module, None)
        };
        assert_eq!(named(&entry, "run"), "run");
        assert_eq!(named(&entry, "$cm_export__run"), "$cm_export__run");
        assert_eq!(
            named(
                &interner.core("prelude/fmt.wado"),
                "write_decimal_digits$sroa[&mut(self.repr)]"
            ),
            "write_decimal_digits"
        );
        // A module path may hold the characters a mangled name separates with.
        assert_eq!(
            named(
                &interner.local("./src (old)/my lib.wado"),
                "follow<core:prelude/string.wado/String>$spec0"
            ),
            "follow<String>"
        );
        // A minted name opens with the prefix; cutting there would leave nothing.
        assert_eq!(
            diagnostic_function_name("$test_0_adds", &entry, None),
            "$test_0_adds"
        );
        assert_eq!(
            diagnostic_function_name(
                &value_copy_helper_name("core:prelude/string.wado/String"),
                &ModuleSource::builtin(),
                None
            ),
            "$value_copy"
        );
        // A type argument carries a module path of its own.
        let traits = interner.core("prelude/traits.wado");
        assert_eq!(
            named(
                &traits,
                "TemplateHole<a.wado/$tmpl$59e1,core:prelude/string.wado/String>::lit$sroa[self.index]"
            ),
            "TemplateHole<$tmpl$59e1,String>::lit"
        );
        let f1 = interner.entry_point("f1.wado");
        assert_eq!(named(&f1, "id<&f1.wado/P>"), "id<&P>");
        assert_eq!(named(&f1, "id<[f1.wado/P,i32]>"), "id<[P,i32]>");
        // A packaged module is named by its package's coordinate.
        assert_eq!(
            named(&f1, "id<acme:dep@0.2.0/src/lib.wado/P,../x.wado/Q>"),
            "id<P,Q>"
        );
        assert_eq!(
            named(
                &f1,
                &format!(
                    "id<{}/P>",
                    packaged_module_name("dep@1.0.0+b.5", "lib.wado")
                )
            ),
            "id<P>"
        );
        assert_eq!(
            named(&f1, "id<fn(a.wado/P)->i32with($e_263)>$$Closure_0"),
            "id<fn(P)->i32with($e_263)>"
        );
        let method = LocalMethodName::with_method_type_args(
            FqTypeName::builtin("Box"),
            None,
            "transform$spec0".into(),
            vec![FqTypeName::builtin("i64")],
        );
        assert_eq!(
            diagnostic_function_name(&method.to_mangled_name(), &entry, Some(&method)),
            "Box::transform<i64>"
        );
    }

    #[test]
    fn type_bridges_are_told_from_plainly_synthesized_functions() {
        assert!(is_type_bridge(&value_copy_helper_name("main.wado/Point")));
        assert!(is_type_bridge(&effect_default_impl_name("Log", "event")));
        assert!(is_type_bridge(
            &MarkerBridge::HoleFmt.shape_name("$tmpl$abc")
        ));
        assert!(!is_type_bridge(MODULE_INIT_FUNCTION));
        assert!(!is_type_bridge("$test_0_adds"));
        assert!(!is_type_bridge("run"));
    }

    #[test]
    fn test_test_name_to_snake_is_ascii_only() {
        // ASCII alphanumerics survive (lowercased); punctuation/space → `_`.
        assert_eq!(test_name_to_snake("Hello, World!"), "hello__world_");
        // Non-ASCII letters collapse to `_` so the kebab export stays valid.
        assert_eq!(test_name_to_snake("café résumé"), "caf__r_sum_");
        assert_eq!(test_name_to_snake("日本語のテスト ok"), "________ok");
        // A fully non-ASCII name leaves no ASCII segment at all.
        assert_eq!(test_name_to_snake("完全に日本語"), "______");
        // The output must never contain a non-ASCII byte.
        for input in ["café résumé", "日本語のテスト ok", "Ω±∞ μ"] {
            assert!(
                test_name_to_snake(input).is_ascii(),
                "snake form of {input:?} must be ASCII"
            );
        }
    }

    #[test]
    fn mangle_local_item_name_is_space_independent() {
        // The storage name only disambiguates local items *within* a module
        // (the module source qualifies it across modules), so it must depend
        // solely on the module-local `local` index — never on the
        // process-global `AstIdSpace`, which varies with unrelated parse
        // history and would otherwise leak into compiler output (mangled WIR
        // function names), making it non-deterministic.
        let space_a = AstIdSpace::next();
        let space_b = AstIdSpace::next();
        assert_ne!(space_a, space_b);
        let a = mangle_local_item_name("UserId", AstId::new(space_a, 19));
        let b = mangle_local_item_name("UserId", AstId::new(space_b, 19));
        assert_eq!(a, b, "mangled name must not encode the AstIdSpace");
        // Distinct declaration sites within a module still get distinct names.
        assert_ne!(a, mangle_local_item_name("UserId", AstId::new(space_a, 20)));
        // The storage name is a rendering of the declared one, not a
        // replacement for it: readers ask the declaration, so nothing reads
        // the declared name back out of this string.
        assert!(a.starts_with("UserId"));
    }

    #[test]
    fn a_packaged_module_keeps_its_unversioned_name_across_versions() {
        assert_eq!(
            unversioned_module_name("acme:app@1.0.0+build.5/src/lib.wado"),
            "acme:app/src/lib.wado"
        );
        for name in [
            "core:collections/hashmap.wado",
            "src/lib.wado",
            "app.wado",
            "https://user@example.com/lib.wado",
        ] {
            assert_eq!(unversioned_module_name(name), name);
        }
    }
}
