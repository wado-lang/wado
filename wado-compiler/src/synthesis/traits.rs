//! Trait synthesis phase.
//!
//! Generates auto-derived trait implementations for types that support them:
//! - `EnumName^Eq::eq(&self, &Self) -> bool` - discriminant equality
//! - `EnumName^Ord::cmp(&self, &Self) -> Ordering` - discriminant ordering
//! - `VariantName^Eq::eq(&self, &Self) -> bool` - case-discriminated payload equality
//! - `TypeName^Inspect::inspect(&self, &mut Formatter)` - debug formatting
//! - `EnumName^Display::fmt(&self, &mut Formatter)` - bare case name
//! - `TypeName^DisplayAlt::fmt_alt(&self, &mut Formatter)` - delegates to `Display`
//!
//! Pipeline position: runs as part of the synthesis phase, before monomorphize.

use std::cell::RefCell;
use std::rc::Rc;

use crate::compiler_item::{CompilerItem, CompilerItems};
use crate::hashmap::IndexSet;

use crate::elaborator::trait_env::TraitEnv;
use crate::module_source::ModuleSource;
use crate::name::{FqTypeName, LocalMethodName, MethodName, Receiver, RefKind};
use crate::package::Package;
use crate::tir::{
    CallArg, FnDispatchTrait, FunctionKind, FunctionRef, InlineHint, MonomorphInfo, ResolvedType,
    TirBinaryOp, TirBlock, TirExpr, TirExprKind, TirFunction, TirLiteralPattern, TirLocal,
    TirMatchArm, TirModule, TirParam, TirPattern, TirStmt, TirStmtKind, TirStructField,
    TirTypeParam, TypeId, TypeTable,
};
use crate::token::Span;

use super::common::{
    deref_expr, make_synthetic_free_function, make_synthetic_method, param_local, ref_expr,
    synth_span, write_str_stmt,
};

/// Snapshot of every `core:prelude/{traits,format}` symbol name that the
/// trait-synthesis phase reaches for. Built once per pass through the
/// [`CompilerItem`] registry and threaded through the helpers so a
/// stdlib rename flows without touching synthesis sites — same shape
/// as [`super::cm_binding::types::CmStdlibNames`] and
/// [`super::template::FormatStdlibNames`].
#[derive(Clone, Debug)]
pub(crate) struct TraitsStdlibNames {
    pub formatter: String,
    /// `formatter` named by its declaring module — the form a function name
    /// embeds. The bare `formatter` stays for type-table lookups.
    pub formatter_fq: FqTypeName,
    pub display: String,
    /// `Display::fmt` method name, resolved via [`Resolved::Trait::method_name`].
    pub display_method: String,
    pub display_alt: String,
    /// `DisplayAlt::fmt_alt` method name, resolved via the registry.
    pub display_alt_method: String,
    pub inspect: String,
    /// `Inspect::inspect` method name, resolved via the registry.
    pub inspect_method: String,
    pub inspect_alt: String,
    /// `InspectAlt::inspect_alt` method name, resolved via the registry.
    pub inspect_alt_method: String,
    pub lower_hex: String,
    /// `LowerHex::fmt` method name, resolved via the registry.
    pub lower_hex_method: String,
    pub less_name: String,
    pub less_index: u32,
    pub equal_name: String,
    pub equal_index: u32,
    pub greater_name: String,
    pub greater_index: u32,
}

impl TraitsStdlibNames {
    pub(crate) fn from_compiler_items(items: &CompilerItems) -> Self {
        let (_, _, less_name, less_index) = items.require_enum_case(CompilerItem::OrderingLess);
        let (_, _, equal_name, equal_index) = items.require_enum_case(CompilerItem::OrderingEqual);
        let (_, _, greater_name, greater_index) =
            items.require_enum_case(CompilerItem::OrderingGreater);
        Self {
            formatter: items.struct_name(CompilerItem::Formatter).to_string(),
            formatter_fq: {
                let (module, name) = items.require_struct(CompilerItem::Formatter);
                FqTypeName::declared(module, name)
            },
            display: items.trait_name(CompilerItem::Display).to_string(),
            display_method: items.trait_method_name(CompilerItem::Display).to_string(),
            display_alt: items.trait_name(CompilerItem::DisplayAlt).to_string(),
            display_alt_method: items
                .trait_method_name(CompilerItem::DisplayAlt)
                .to_string(),
            inspect: items.trait_name(CompilerItem::Inspect).to_string(),
            inspect_method: items.trait_method_name(CompilerItem::Inspect).to_string(),
            inspect_alt: items.trait_name(CompilerItem::InspectAlt).to_string(),
            inspect_alt_method: items
                .trait_method_name(CompilerItem::InspectAlt)
                .to_string(),
            lower_hex: items.trait_name(CompilerItem::LowerHex).to_string(),
            lower_hex_method: items.trait_method_name(CompilerItem::LowerHex).to_string(),
            less_name: less_name.to_string(),
            less_index,
            equal_name: equal_name.to_string(),
            equal_index,
            greater_name: greater_name.to_string(),
            greater_index,
        }
    }
}

/// One half of an auto-derived trait pair: the `Display`/`DisplayAlt`/`InspectAlt`
/// fallback machinery is parameterised over which trait to emit and which trait
/// to delegate to.
///
/// Every name in this struct — both trait names and their method names —
/// flows from the `CompilerItem` registry. The stdlib's
/// `#[compiler_item("...")]` annotations control the spelling on both
/// halves, so renaming `Display::fmt` to `Display::display_value` flows
/// to the synthesised fallback's emitted call without touching this
/// code.
struct TraitPair {
    /// e.g. `"Display"` or `"DisplayAlt"`.
    target_trait: String,
    /// e.g. `"fmt"` or `"fmt_alt"`.
    target_method: String,
    /// Trait the fallback delegates to (`"Display"`).
    delegate_trait: String,
    /// Method on the delegate trait (`"fmt"`).
    delegate_method: String,
}

impl TraitPair {
    fn display_alt(names: &TraitsStdlibNames) -> Self {
        // The alternate *display* of a value defaults to its plain display,
        // mirroring Rust's `{:#}` vs `{:#?}`; pretty-printing stays on
        // `InspectAlt`. `Display` itself has no fallback — a type without an
        // `impl Display` is a `${x}` error pointing at `${x:?}`.
        Self {
            target_trait: names.display_alt.clone(),
            target_method: names.display_alt_method.clone(),
            delegate_trait: names.display.clone(),
            delegate_method: names.display_method.clone(),
        }
    }
}

/// Shorthand for `LocalMethodName::new(struct.into(), Some(trait.into()), method.into())`.
///
/// Leaves `base_trait_module` as `None`: every synthesis caller is
/// auto-deriving an impl for a project-globally-unique core trait
/// (Inspect / Display / Eq / Ord / From / `serde` adapters, …) whose
/// name dispatch synthesis recognises without needing the disambiguating
/// module. The elaborator path that lifts user-written
/// `impl <Trait> for <Type>` blocks populates the module via
/// `Elaborator::canonical_decl_key` directly into the [`LocalMethodName`]
/// struct literal.
/// The receiver is named by the module that declares it, the same rule a
/// resolved type follows when mangled into any other name.
fn trait_method_info(
    module_source: &ModuleSource,
    struct_name: &str,
    trait_name: &str,
    method: &str,
) -> LocalMethodName {
    LocalMethodName::new(
        FqTypeName::of_head(module_source, struct_name),
        Some(trait_name.to_string()),
        method.to_string(),
    )
}

/// Build a local-variable reference expression.
fn local_expr(index: u32, name: &str, type_id: TypeId, span: Span) -> TirExpr {
    TirExpr::new(
        TirExprKind::Local {
            index,
            name: name.to_string(),
        },
        type_id,
        span,
    )
}

/// Build `*<local>` where the local has reference type `ref_type` and dereferences to `inner_type`.
fn deref_local(
    index: u32,
    name: &str,
    ref_type: TypeId,
    inner_type: TypeId,
    span: Span,
) -> TirExpr {
    deref_expr(local_expr(index, name, ref_type, span), inner_type, span)
}

/// The leading `self` parameter (`local_index 0`) of a synthesized method,
/// typed as the receiver reference `ref_type`.
fn self_param(ref_type: TypeId, span: Span) -> TirParam {
    TirParam {
        name: "self".to_string(),
        type_id: ref_type,
        local_index: 0,
        is_mut: false,
        is_mut_ref: false,
        span,
    }
}

/// Standard `(self, other)` parameter list for `Eq`/`Ord`-style methods.
fn binary_method_params(ref_type: TypeId, span: Span) -> Vec<TirParam> {
    vec![
        self_param(ref_type, span),
        TirParam {
            name: "other".to_string(),
            type_id: ref_type,
            local_index: 1,
            is_mut: false,
            is_mut_ref: false,
            span,
        },
    ]
}

/// Locals slice matching `binary_method_params`.
fn binary_method_locals(ref_type: TypeId) -> Vec<TirLocal> {
    vec![
        param_local("self", ref_type, false),
        param_local("other", ref_type, false),
    ]
}

/// Standard `(&self, &mut Formatter)` parameter list for `Inspect`/`Display`-style methods.
fn inspect_params(ref_type: TypeId, fmt_type: TypeId, span: Span) -> Vec<TirParam> {
    vec![
        self_param(ref_type, span),
        TirParam {
            name: "f".to_string(),
            type_id: fmt_type,
            local_index: 1,
            is_mut: false,
            is_mut_ref: false,
            span,
        },
    ]
}

/// Locals slice matching `inspect_params`.
fn inspect_locals(ref_type: TypeId, fmt_type: TypeId) -> Vec<TirLocal> {
    vec![
        param_local("self", ref_type, false),
        param_local("f", fmt_type, false),
    ]
}

/// Build an `Ordering::<case>` enum-construct expression.
fn ordering_construct(
    ordering_type: TypeId,
    case_index: u32,
    case_name: &str,
    span: Span,
) -> TirExpr {
    TirExpr::new(
        TirExprKind::EnumConstruct {
            enum_type: ordering_type,
            case_index,
            case_name: case_name.to_string(),
        },
        ordering_type,
        span,
    )
}

/// Like `make_synthetic_method`, but lets the caller attach `impl_type_params`
/// so generic auto-derived methods participate in monomorphisation.
fn make_trait_method(
    name: String,
    method_info: LocalMethodName,
    impl_type_params: Vec<TirTypeParam>,
    params: Vec<TirParam>,
    return_type: TypeId,
    body: TirBlock,
    locals: Vec<TirLocal>,
    span: Span,
) -> TirFunction {
    let local_count = locals.len() as u32;
    TirFunction {
        module_source: ModuleSource::default(),
        name,
        visibility: crate::ast::Visibility::Public,
        is_export: false,
        is_async: false,
        type_params: Vec::new(),
        impl_type_params,
        monomorph_info: None,
        method_info: Some(method_info),
        params,
        return_type,
        task_return_type: None,
        effects: Vec::new(),
        stores: vec![],
        body: Some(body),
        span,
        local_count,
        locals,
        address_taken_locals: IndexSet::default(),
        stores_aliased_locals: IndexSet::default(),
        is_cm_binding: false,
        is_dispatch_wrapper: false,
        is_cm_export: false,
        is_ambient: false,
        benign_effects: Vec::new(),
        inline_hint: InlineHint::Auto,
        compiler_item: None,
        export_name: None,
        allocator_tag: None,
        kind: FunctionKind::Regular,

        return_abi: crate::tir::ReturnAbi::default(),
    }
}

/// Run trait synthesis on the entire project.
///
/// For each module, generates Eq/Ord, Inspect, `InspectAlt`, Display, and `DisplayAlt`
/// implementations for types that don't already have user-provided implementations.
pub fn synthesize_traits(project: Package) -> Package {
    let mut project = project;
    let trait_env = project.trait_env.clone();

    // Bound-driven requests (WEP 2026-06-25-trait-derivation): snapshot the
    // shared set (not a drain — `synthesis::serde_synth::synthesize_serde`
    // reads the same set for its `Serialize` / `Deserialize` entries after
    // this pass runs) and keep only the `Eq` / `Ord` entries.
    //
    // `TypeTable` is shared (one `Rc<RefCell<…>>` per project, cloned onto
    // every module), so any module's handle reaches the same table, and
    // `build_tir` always populates at least the entry module first.
    let first_module = project
        .tir_modules
        .values()
        .next()
        .expect("tir_modules must contain at least the entry module during synthesis");
    let (eq_trait_name, ord_trait_name) = {
        let tt = first_module.type_table.borrow();
        let items = tt.compiler_items();
        (
            items.trait_name(CompilerItem::Eq).to_string(),
            items.trait_name(CompilerItem::Ord).to_string(),
        )
    };
    // `Default` is drained later by `synthesize_defaults` (after `serde_synth`).
    let requested: IndexSet<(String, ModuleSource, String)> = first_module
        .type_table
        .borrow()
        .bound_driven_synth_requests(|trait_name| {
            trait_name == eq_trait_name || trait_name == ord_trait_name
        })
        .into_iter()
        .collect();

    // In-pass dedup: each sub-pass records `(type_name, module, trait_name)`
    // of every impl it generates so later sub-passes within this same
    // `synthesize_traits` run can skip emitting a duplicate. The module
    // component is required because two distinct types from different
    // modules can share a simple name (e.g. `struct Widget` in module A
    // and module B), and each needs its own auto-derived impl. The
    // canonical project-wide synthesis layer is rebuilt afterwards by
    // `collect_synthesised_impls` (see `synthesis.rs`), which scans TIR
    // and captures concrete-ness from the synthesized function itself.
    let mut pending: IndexSet<(String, ModuleSource, String)> = IndexSet::default();
    for module in project.tir_modules.values_mut() {
        let module_source = module.module_source.clone();
        let names = {
            let tt = module.type_table.borrow();
            TraitsStdlibNames::from_compiler_items(tt.compiler_items())
        };
        let mut ctx = SynthesisCtx {
            trait_env: &trait_env,
            pending: &mut pending,
            requested: &requested,
            module: module_source.clone(),
            names: &names,
        };
        generate_enum_trait_impls(module, &mut ctx);
        generate_struct_eq_ord_impls(module, &mut ctx);
        generate_variant_eq_impls(module, &mut ctx);
        generate_inspect_impls(module, &mut ctx);
        generate_inspect_alt_impls(module, &mut ctx);
        // `Display` is auto-derived only for plain `enum`s (the bare case name).
        // A newtype inherits its base's `Display` at the format call site
        // (`peel_transparent_newtype`), not here. Runs before the `DisplayAlt`
        // pass, whose `needs_fallback` reads the recorded `Display` impl.
        generate_enum_display_impls(module, &mut ctx);
        generate_display_alt_fallback_impls(module, &mut ctx);
    }
    project
}

/// Generate `Struct^Default::default()` for each requested defaults-eligible
/// struct. A separate pass from `synthesize_traits` because `serde_synth`'s
/// `Deserialize` bodies record `Default` requests only after that snapshot.
pub fn synthesize_defaults(project: &mut Package) {
    let trait_env = project.trait_env.clone();
    let first_module = project
        .tir_modules
        .values()
        .next()
        .expect("tir_modules must contain at least the entry module during synthesis");
    let default_trait_name = first_module
        .type_table
        .borrow()
        .compiler_items()
        .trait_name(CompilerItem::Default)
        .to_string();
    let requested: IndexSet<(String, ModuleSource, String)> = first_module
        .type_table
        .borrow()
        .bound_driven_synth_requests(|trait_name| trait_name == default_trait_name)
        .into_iter()
        .collect();

    let mut pending: IndexSet<(String, ModuleSource, String)> = IndexSet::default();
    for module in project.tir_modules.values_mut() {
        let module_source = module.module_source.clone();
        let names = {
            let tt = module.type_table.borrow();
            TraitsStdlibNames::from_compiler_items(tt.compiler_items())
        };
        let mut ctx = SynthesisCtx {
            trait_env: &trait_env,
            pending: &mut pending,
            requested: &requested,
            module: module_source.clone(),
            names: &names,
        };
        generate_struct_default_impls(module, &mut ctx);
    }
}

/// Generate `Struct^ReflectStruct::type_name()` for each eligible struct
/// (WEP 2026-06-13 §1).
pub fn synthesize_reflect(project: &mut Package) {
    let trait_env = project.trait_env.clone();
    let first_module = project
        .tir_modules
        .values()
        .next()
        .expect("tir_modules must contain at least the entry module during synthesis");
    let reflect_trait_name = first_module
        .type_table
        .borrow()
        .compiler_items()
        .trait_name(CompilerItem::ReflectStruct)
        .to_string();
    // Not demand-driven: `TypeTable::is_reflect_eligible` decides coverage, and
    // the bound check reads the same predicate.
    let requested: IndexSet<(String, ModuleSource, String)> = IndexSet::default();

    let mut pending: IndexSet<(String, ModuleSource, String)> = IndexSet::default();
    for module in project.tir_modules.values_mut() {
        let module_source = module.module_source.clone();
        let names = {
            let tt = module.type_table.borrow();
            TraitsStdlibNames::from_compiler_items(tt.compiler_items())
        };
        let mut ctx = SynthesisCtx {
            trait_env: &trait_env,
            pending: &mut pending,
            requested: &requested,
            module: module_source,
            names: &names,
        };
        generate_struct_reflect_impls(module, &mut ctx, &reflect_trait_name);
    }
}

/// Synthesize the `ReflectStruct` impl of every requested struct in `module`.
fn generate_struct_reflect_impls(
    module: &mut TirModule,
    ctx: &mut SynthesisCtx<'_, '_, '_>,
    reflect_trait_name: &str,
) {
    if module.structs.is_empty() {
        return;
    }

    let targets = collect_reflect_targets(module);
    if targets.is_empty() {
        return;
    }

    let module_source = module.module_source.clone();
    let env = ReflectSynthEnv::resolve(&mut module.type_table.borrow_mut());

    let mut generated = Vec::new();
    for target in &targets {
        let tt = module.type_table.borrow();
        let eligible = tt
            .find_decl_type_by_name(&target.name, &module_source)
            .is_some_and(|type_id| tt.is_reflect_eligible(type_id));
        drop(tt);
        if !eligible {
            continue;
        }
        let methods = generate_struct_reflect_methods(
            &module.type_table,
            &env,
            &module_source,
            reflect_trait_name,
            target,
        );
        generated.extend(methods.into_iter().map(|f| Rc::new(RefCell::new(f))));
        ctx.record_impl(&target.name, reflect_trait_name);
    }

    module.functions.extend(generated);
}

/// Per-field info a struct's `ReflectStruct` synthesis needs: identity plus the
/// serde / secret / default metadata baked into each `Field` member.
struct ReflectFieldInfo {
    name: String,
    type_id: TypeId,
    index: u32,
    wire_name_override: Option<String>,
    is_secret: bool,
    has_default: bool,
}

/// A struct selected for `ReflectStruct` synthesis. `type_params` is empty for
/// a plain struct; a generic one gets a single impl over `S<T, …>`.
struct ReflectTarget {
    name: String,
    type_params: Vec<TirTypeParam>,
    fields: Vec<ReflectFieldInfo>,
    wire_name_policy: Option<String>,
    span: Span,
}

/// Select the structs in `module` that need a synthesized `ReflectStruct` impl:
/// every declared struct, generic or not. Monomorphized instances inherit the
/// generic impl through substitution.
fn collect_reflect_targets(module: &TirModule) -> Vec<ReflectTarget> {
    module
        .structs
        .iter()
        .filter(|s| s.monomorph_info.is_none())
        .map(|s| ReflectTarget {
            name: s.name.clone(),
            type_params: s.type_params.clone(),
            fields: s
                .fields
                .iter()
                .map(|f| ReflectFieldInfo {
                    name: f.name.clone(),
                    type_id: f.type_id,
                    index: f.index,
                    wire_name_override: f.wire_name_override.clone(),
                    is_secret: f.is_secret,
                    has_default: f.default_expr.is_some(),
                })
                .collect(),
            wire_name_policy: s.wire_name_policy.clone(),
            span: s.span,
        })
        .collect()
}

/// Synthesize one struct's `type_name()` and `members()` methods, register
/// its `FieldTypes` / `Members` associated tuple types, and emit the
/// per-field-type `$field_get$S$F` bridge helpers.
fn generate_struct_reflect_methods(
    type_table: &RefCell<TypeTable>,
    env: &ReflectSynthEnv,
    module_source: &ModuleSource,
    reflect_trait_name: &str,
    target: &ReflectTarget,
) -> Vec<TirFunction> {
    let ReflectTarget {
        name,
        type_params,
        fields,
        wire_name_policy: name_policy,
        span,
    } = target;
    let span = *span;
    let is_generic = !type_params.is_empty();
    let field_infos: Vec<FieldInfo> = fields
        .iter()
        .map(|f| (f.name.clone(), f.type_id, f.index))
        .collect();

    let type_name_fn = generate_type_name_fn(
        module_source,
        name,
        env.string_type,
        reflect_trait_name,
        &env.type_name_method,
        span,
    );

    let (struct_type, ref_struct_type, member_types, members_tuple_type, fields_tuple_type) = {
        let mut tt = type_table.borrow_mut();
        let struct_type = if is_generic {
            let param_ids = make_type_param_ids(type_params, &mut tt);
            tt.make_generic_instance(name.clone(), module_source.clone(), param_ids)
        } else {
            tt.make_struct(name.clone(), module_source.clone())
        };
        let ref_struct_type = tt.make_ref(struct_type);
        let fields_tuple_type = tt.make_tuple(fields.iter().map(|f| f.type_id).collect());
        let member_types: Vec<TypeId> = fields
            .iter()
            .map(|f| {
                tt.make_generic_instance(
                    env.member_struct_name.clone(),
                    env.member_struct_module.clone(),
                    vec![struct_type, f.type_id],
                )
            })
            .collect();
        let members_tuple_type = tt.make_tuple(member_types.clone());
        register_reflect_assoc_types(
            &mut tt,
            struct_type,
            CompilerItem::ReflectStruct,
            is_generic,
            &[
                (REFLECT_FIELD_TYPES_ASSOC, fields_tuple_type),
                (REFLECT_MEMBERS_ASSOC, members_tuple_type),
            ],
        );
        (
            struct_type,
            ref_struct_type,
            member_types,
            members_tuple_type,
            fields_tuple_type,
        )
    };

    let members_fn = generate_struct_members_fn(
        module_source,
        type_table,
        env,
        reflect_trait_name,
        name,
        fields,
        &member_types,
        members_tuple_type,
        span,
    );
    let from_fields_fn = generate_struct_from_fields_fn(
        module_source,
        env,
        reflect_trait_name,
        name,
        struct_type,
        fields,
        fields_tuple_type,
        span,
    );
    let wire_name_policy_fn = generate_wire_name_policy_fn(
        module_source,
        name,
        env.case_style_type,
        name_policy,
        reflect_trait_name,
        &env.wire_name_policy_method,
        span,
    );

    let mut functions = vec![
        type_name_fn,
        members_fn,
        from_fields_fn,
        wire_name_policy_fn,
    ];
    for f in &mut functions {
        f.impl_type_params.clone_from(type_params);
    }
    // A generic struct's bridges are keyed by concrete field types, so
    // `synthesize_monomorphized_reflect_bridges` mints them per instantiation.
    if !is_generic {
        functions.extend(generate_field_bridge_helpers(
            type_table,
            &field_infos,
            struct_type,
            ref_struct_type,
            span,
        ));
    }
    functions
}

/// Record a reflect kind's associated tuple types for `self_type`: resolved
/// directly for a non-generic type, registered as generic definitions keyed by
/// the declaring `AstId` for a generic one.
fn register_reflect_assoc_types(
    tt: &mut TypeTable,
    self_type: TypeId,
    owning_trait: CompilerItem,
    is_generic: bool,
    assocs: &[(&str, TypeId)],
) {
    let base_decl = tt.decl_of_type(self_type);
    let trait_name = tt.compiler_items().trait_name(owning_trait).to_string();
    for (assoc_name, resolved) in assocs {
        if is_generic {
            let Some(base_decl) = base_decl else { continue };
            tt.register_generic_assoc_type_def(
                base_decl,
                trait_name.clone(),
                (*assoc_name).to_string(),
                *resolved,
            );
        } else {
            tt.register_assoc_type_resolution(
                self_type,
                trait_name.clone(),
                (*assoc_name).to_string(),
                *resolved,
            );
        }
    }
}

/// The member-tuple associated type, spelled `Members` on every reflect trait.
/// Sealed and compiler-defined, so its spelling is fixed rather than
/// registry-driven.
pub(crate) const REFLECT_MEMBERS_ASSOC: &str = "Members";

/// `ReflectStruct`'s payload-pack associated type (`type FieldTypes`): the
/// field types themselves, bound to place per-field trait bounds on a
/// derivation.
pub(crate) const REFLECT_FIELD_TYPES_ASSOC: &str = "FieldTypes";

/// Module-level types and method names resolved once from the compiler-item
/// registry and reused across every struct's `ReflectStruct` synthesis in that
/// module.
struct ReflectSynthEnv {
    string_type: TypeId,
    case_style_type: TypeId,
    member_struct_name: String,
    member_struct_module: ModuleSource,
    type_name_method: String,
    members_method: String,
    from_fields_method: String,
    wire_name_policy_method: String,
}

impl ReflectSynthEnv {
    fn resolve(tt: &mut TypeTable) -> Self {
        let string_type = tt.make_compiler_struct(CompilerItem::String);
        let case_style_type = tt.make_compiler_enum(CompilerItem::CaseStyle);
        let items = tt.compiler_items();
        let (member_struct_module, member_struct_name) = {
            let (m, n) = items.require_struct(CompilerItem::ReflectStructField);
            (m.clone(), n.to_string())
        };
        Self {
            string_type,
            case_style_type,
            member_struct_name,
            member_struct_module,
            type_name_method: items
                .method_name(CompilerItem::ReflectStructTypeName)
                .to_string(),
            members_method: items
                .method_name(CompilerItem::ReflectStructMembers)
                .to_string(),
            from_fields_method: items
                .method_name(CompilerItem::ReflectStructFromFields)
                .to_string(),
            wire_name_policy_method: items
                .method_name(CompilerItem::ReflectStructWireNamePolicy)
                .to_string(),
        }
    }
}

/// Shared driver for the kind-specific reflection syntheses (`ReflectVariant` /
/// `ReflectEnum` / `ReflectFlags`): resolve the trait's bound-driven synth
/// requests once, then run `generate_impls` per module. Only the trait item and
/// the per-module impl generator differ between the three kinds.
fn synthesize_reflect_kind(
    project: &mut Package,
    trait_item: CompilerItem,
    generate_impls: fn(&mut TirModule, &mut SynthesisCtx<'_, '_, '_>, &str),
) {
    let trait_env = project.trait_env.clone();
    let first_module = project
        .tir_modules
        .values()
        .next()
        .expect("tir_modules must contain at least the entry module during synthesis");
    let trait_name = first_module
        .type_table
        .borrow()
        .compiler_items()
        .trait_name(trait_item)
        .to_string();
    let requested: IndexSet<(String, ModuleSource, String)> = first_module
        .type_table
        .borrow()
        .bound_driven_synth_requests(|t| t == trait_name)
        .into_iter()
        .collect();

    let mut pending: IndexSet<(String, ModuleSource, String)> = IndexSet::default();
    for module in project.tir_modules.values_mut() {
        let module_source = module.module_source.clone();
        let names = {
            let tt = module.type_table.borrow();
            TraitsStdlibNames::from_compiler_items(tt.compiler_items())
        };
        let mut ctx = SynthesisCtx {
            trait_env: &trait_env,
            pending: &mut pending,
            requested: &requested,
            module: module_source.clone(),
            names: &names,
        };
        generate_impls(module, &mut ctx, &trait_name);
    }
}

/// `members()` for a payload-free kind: one member struct literal per case /
/// bit, packed into the trait's `Members` tuple.
fn generate_reflect_member_tuple_fn(
    method_info: LocalMethodName,
    member_struct_name: &str,
    member_type: TypeId,
    members_tuple_type: TypeId,
    rows: Vec<Vec<TirStructField>>,
    span: Span,
) -> TirFunction {
    let qualified_name = method_info.to_mangled_name();

    let elements = rows
        .into_iter()
        .map(|fields| {
            TirExpr::new(
                TirExprKind::StructLiteral {
                    struct_type: member_type,
                    struct_name: member_struct_name.to_string(),
                    fields,
                },
                member_type,
                span,
            )
        })
        .collect();
    let tuple = TirExpr::new(
        TirExprKind::TupleLiteral { elements },
        members_tuple_type,
        span,
    );

    let body = TirBlock::new(
        vec![TirStmt::new(
            TirStmtKind::Return { value: Some(tuple) },
            span,
        )],
        span,
    );

    make_synthetic_method(
        qualified_name,
        method_info,
        vec![],
        members_tuple_type,
        body,
        vec![],
    )
}

/// A metadata-struct field holding an integer literal of type `ty`.
fn reflect_meta_int_field(
    name: &str,
    value: u64,
    ty: TypeId,
    index: u32,
    span: Span,
) -> TirStructField {
    TirStructField {
        name: name.to_string(),
        value: TirExpr::new(
            TirExprKind::IntLiteral {
                value,
                repr: value.to_string(),
            },
            ty,
            span,
        ),
        field_index: index,
    }
}

/// Build `Struct^ReflectStruct::members()` as
/// `return [Field { index: 0, field_name: "f", wire_override: …, has_default: …,
/// is_secret: … }, …];` — one fat member per field, each typed `StructField<S, F_k>`.
#[allow(clippy::too_many_arguments)]
fn generate_struct_members_fn(
    module_source: &ModuleSource,
    type_table: &RefCell<TypeTable>,
    env: &ReflectSynthEnv,
    reflect_trait_name: &str,
    struct_name: &str,
    fields: &[ReflectFieldInfo],
    member_types: &[TypeId],
    members_tuple_type: TypeId,
    span: Span,
) -> TirFunction {
    let method_info = trait_method_info(
        module_source,
        struct_name,
        reflect_trait_name,
        &env.members_method,
    );
    let qualified_name = method_info.to_mangled_name();

    let option_string_type = type_table.borrow_mut().make_option(env.string_type);

    let elements = fields
        .iter()
        .zip(member_types)
        .map(|(f, member_type)| {
            let wire_override = {
                let tt = type_table.borrow();
                let items = tt.compiler_items();
                match &f.wire_name_override {
                    Some(rename) => crate::synthesis::common::option_some(
                        TirExpr::new(
                            TirExprKind::StringLiteral(rename.clone()),
                            env.string_type,
                            span,
                        ),
                        option_string_type,
                        items,
                    ),
                    None => crate::synthesis::common::option_none(option_string_type, items),
                }
            };
            let field_fields = vec![
                reflect_meta_int_field("index", u64::from(f.index), TypeTable::I32, 0, span),
                TirStructField {
                    name: "field_name".to_string(),
                    value: TirExpr::new(
                        TirExprKind::StringLiteral(f.name.clone()),
                        env.string_type,
                        span,
                    ),
                    field_index: 1,
                },
                TirStructField {
                    name: "wire_override".to_string(),
                    value: wire_override,
                    field_index: 2,
                },
                TirStructField {
                    name: "has_default".to_string(),
                    value: TirExpr::new(
                        TirExprKind::BoolLiteral(f.has_default),
                        TypeTable::BOOL,
                        span,
                    ),
                    field_index: 3,
                },
                TirStructField {
                    name: "is_secret".to_string(),
                    value: TirExpr::new(
                        TirExprKind::BoolLiteral(f.is_secret),
                        TypeTable::BOOL,
                        span,
                    ),
                    field_index: 4,
                },
            ];
            TirExpr::new(
                TirExprKind::StructLiteral {
                    struct_type: *member_type,
                    struct_name: env.member_struct_name.clone(),
                    fields: field_fields,
                },
                *member_type,
                span,
            )
        })
        .collect();
    let tuple = TirExpr::new(
        TirExprKind::TupleLiteral { elements },
        members_tuple_type,
        span,
    );

    let body = TirBlock::new(
        vec![TirStmt::new(
            TirStmtKind::Return { value: Some(tuple) },
            span,
        )],
        span,
    );

    make_synthetic_method(
        qualified_name,
        method_info,
        vec![],
        members_tuple_type,
        body,
        vec![],
    )
}

/// Build `S^ReflectStruct::from_fields(fields: [F_0, …]) -> S`:
/// `return S { f_0: fields.0, … };`.
fn generate_struct_from_fields_fn(
    module_source: &ModuleSource,
    env: &ReflectSynthEnv,
    reflect_trait_name: &str,
    struct_name: &str,
    struct_type: TypeId,
    fields: &[ReflectFieldInfo],
    fields_tuple_type: TypeId,
    span: Span,
) -> TirFunction {
    let method_info = trait_method_info(
        module_source,
        struct_name,
        reflect_trait_name,
        &env.from_fields_method,
    );
    let qualified_name = method_info.to_mangled_name();

    let literal_fields = fields
        .iter()
        .enumerate()
        .map(|(position, f)| TirStructField {
            name: f.name.clone(),
            value: TirExpr::new(
                TirExprKind::FieldAccess {
                    expr: Box::new(local_expr(0, "fields", fields_tuple_type, span)),
                    field_index: position as u32,
                    field_name: position.to_string(),
                },
                f.type_id,
                span,
            ),
            field_index: f.index,
        })
        .collect();

    let literal = TirExpr::new(
        TirExprKind::StructLiteral {
            struct_type,
            struct_name: struct_name.to_string(),
            fields: literal_fields,
        },
        struct_type,
        span,
    );

    let body = TirBlock::new(
        vec![TirStmt::new(
            TirStmtKind::Return {
                value: Some(literal),
            },
            span,
        )],
        span,
    );

    make_synthetic_method(
        qualified_name,
        method_info,
        vec![TirParam {
            name: "fields".to_string(),
            type_id: fields_tuple_type,
            local_index: 0,
            is_mut: false,
            is_mut_ref: false,
            span,
        }],
        struct_type,
        body,
        vec![],
    )
}

/// Map a `#[wire(name_policy)]` string to its `CaseStyle` case
/// `(index, name)`. Mirrors `serde_synth::apply_rename_all`'s recognised
/// strategies; any unknown string (and no attribute) falls back to `Identity`.
fn case_style_variant(name_policy: &Option<String>) -> (u32, &'static str) {
    match name_policy.as_deref() {
        None => (0, "Identity"),
        Some("camelCase") => (1, "Camel"),
        Some("snake_case") => (2, "Snake"),
        Some("SCREAMING_SNAKE_CASE") => (3, "ScreamingSnake"),
        Some("PascalCase") => (4, "Pascal"),
        Some("kebab-case") => (5, "Kebab"),
        Some("SCREAMING-KEBAB-CASE") => (6, "ScreamingKebab"),
        Some(_) => (0, "Identity"),
    }
}

/// Build `T^Reflect*::wire_name_policy() -> CaseStyle` as
/// `return CaseStyle::<variant>;` — the type's `#[wire(name_policy)]` as a
/// `CaseStyle` value (casing itself is resolved library-side). Shared by all
/// four reflect kinds.
fn generate_wire_name_policy_fn(
    module_source: &ModuleSource,
    type_name: &str,
    case_style_type: TypeId,
    name_policy: &Option<String>,
    reflect_trait_name: &str,
    wire_name_policy_method: &str,
    span: Span,
) -> TirFunction {
    let method_info = trait_method_info(
        module_source,
        type_name,
        reflect_trait_name,
        wire_name_policy_method,
    );
    let qualified_name = method_info.to_mangled_name();

    let (case_index, case_name) = case_style_variant(name_policy);
    let construct = TirExpr::new(
        TirExprKind::EnumConstruct {
            enum_type: case_style_type,
            case_index,
            case_name: case_name.to_string(),
        },
        case_style_type,
        span,
    );
    let body = TirBlock::new(
        vec![TirStmt::new(
            TirStmtKind::Return {
                value: Some(construct),
            },
            span,
        )],
        span,
    );

    make_synthetic_method(
        qualified_name,
        method_info,
        vec![],
        case_style_type,
        body,
        vec![],
    )
}

/// Synthesize the `$field_get$S$F` helpers for every distinct field type of a
/// struct. `StructField::<S, F>::get`'s body carries a `builtin::struct_field_get`
/// marker; lowering rewrites each monomorphized marker to its helper
/// (WEP 2026-06-13 §2). Extract-only and guard-free — every struct field is
/// always present.
pub(super) fn generate_field_bridge_helpers(
    type_table: &RefCell<TypeTable>,
    fields: &[FieldInfo],
    struct_type: TypeId,
    ref_struct_type: TypeId,
    span: Span,
) -> Vec<TirFunction> {
    let mangled_struct = type_table.borrow().mangle_type_arg_for_generic(struct_type);

    // Key each helper by the field type's *erased* mangle. `erase_newtypes_and_flags`
    // (after monomorphize, before lower) collapses `Newtype`/`Flags` to their base,
    // and the `struct_field_get` call site mangles its `field_ty` through that
    // erasure — so a helper minted here under the pre-erasure newtype name would
    // never be found. Fields sharing an erased type share one index-dispatched
    // helper.
    let mut by_field_type: crate::hashmap::IndexMap<String, (TypeId, Vec<(String, u32)>)> =
        crate::hashmap::IndexMap::default();
    for (field_name, field_type, index) in fields {
        let mangled = type_table.borrow().mangle_type_arg_erased(*field_type);
        by_field_type
            .entry(mangled)
            .or_insert_with(|| (*field_type, Vec::new()))
            .1
            .push((field_name.clone(), *index));
    }

    let mut helpers = Vec::new();
    for (mangled_field, (field_type, group)) in &by_field_type {
        helpers.push(generate_field_get_helper(
            crate::name::field_get_helper_name(&mangled_struct, mangled_field),
            ref_struct_type,
            *field_type,
            group,
            span,
        ));
    }
    helpers
}

/// Build `$field_get$S$F(v: &S, index: i32) -> F`:
/// `return match index { field_index => v.<field_name>, … _ => unreachable() };`.
fn generate_field_get_helper(
    helper_name: String,
    ref_struct_type: TypeId,
    field_type: TypeId,
    fields: &[(String, u32)],
    span: Span,
) -> TirFunction {
    let dispatch = case_index_dispatch(
        local_expr(1, "index", TypeTable::I32, span),
        fields,
        |field_name, index| {
            field_access_local(0, "v", ref_struct_type, index, field_name, field_type, span)
        },
        field_type,
        span,
    );

    let body = TirBlock::new(
        vec![TirStmt::new(
            TirStmtKind::Return {
                value: Some(dispatch),
            },
            span,
        )],
        span,
    );

    make_synthetic_free_function(
        helper_name,
        vec![
            TirParam {
                name: "v".to_string(),
                type_id: ref_struct_type,
                local_index: 0,
                is_mut: false,
                is_mut_ref: false,
                span,
            },
            TirParam {
                name: "index".to_string(),
                type_id: TypeTable::I32,
                local_index: 1,
                is_mut: false,
                is_mut_ref: false,
                span,
            },
        ],
        field_type,
        body,
        vec![
            param_local("v", ref_struct_type, false),
            param_local("index", TypeTable::I32, false),
        ],
    )
}

/// Build `Type^ReflectKind::type_name() -> String { return "Type"; }` —
/// shared by the struct `ReflectStruct` and variant `ReflectVariant` syntheses.
fn generate_type_name_fn(
    module_source: &ModuleSource,
    struct_name: &str,
    string_type: TypeId,
    reflect_trait_name: &str,
    type_name_method: &str,
    span: Span,
) -> TirFunction {
    let method_info = trait_method_info(
        module_source,
        struct_name,
        reflect_trait_name,
        type_name_method,
    );
    let qualified_name = method_info.to_mangled_name();

    let literal = TirExpr::new(
        TirExprKind::StringLiteral(struct_name.to_string()),
        string_type,
        span,
    );
    let body = TirBlock::new(
        vec![TirStmt::new(
            TirStmtKind::Return {
                value: Some(literal),
            },
            span,
        )],
        span,
    );

    make_synthetic_method(
        qualified_name,
        method_info,
        vec![],
        string_type,
        body,
        vec![],
    )
}

/// Generate the `ReflectVariant` members for each requested variant
/// (WEP 2026-06-13 §3d): `type_name()`, `discriminant(&self)`, `cases()`, plus
/// the per-payload `extract` / `construct` helpers.
pub fn synthesize_reflect_variant(project: &mut Package) {
    synthesize_reflect_kind(
        project,
        CompilerItem::ReflectVariant,
        generate_variant_reflect_impls,
    );
}

/// Synthesize the `ReflectVariant` impl of every requested variant in `module`.
fn generate_variant_reflect_impls(
    module: &mut TirModule,
    ctx: &mut SynthesisCtx<'_, '_, '_>,
    variant_trait_name: &str,
) {
    if module.variants.is_empty() {
        return;
    }

    let targets = collect_reflect_variant_targets(module);
    if targets.is_empty() {
        return;
    }

    let module_source = module.module_source.clone();
    let env = ReflectVariantSynthEnv::resolve(&mut module.type_table.borrow_mut());

    let mut generated = Vec::new();
    for target in &targets {
        let methods = generate_variant_reflect_methods(
            &module.type_table,
            &env,
            &module_source,
            variant_trait_name,
            target,
        );
        generated.extend(methods.into_iter().map(|f| Rc::new(RefCell::new(f))));
        ctx.record_impl(&target.name, variant_trait_name);
    }

    module.functions.extend(generated);
}

/// A variant selected for `ReflectVariant` synthesis. `type_params` is empty
/// for a plain variant; a generic one gets a single impl over `V<T, …>`.
struct ReflectVariantTarget {
    name: String,
    type_params: Vec<TirTypeParam>,
    /// Per-case `(name, index, payload type, #[wire(name)])`; unit cases
    /// carry `()` as their payload.
    cases: Vec<(String, u32, TypeId, Option<String>)>,
    span: Span,
    wire_name_policy: Option<String>,
}

/// Select the variants in `module` that need a synthesized `ReflectVariant`
/// impl: every eligible declaration, generic or not.
fn collect_reflect_variant_targets(module: &TirModule) -> Vec<ReflectVariantTarget> {
    module
        .variants
        .iter()
        .filter(|v| {
            let tt = module.type_table.borrow();
            tt.find_decl_type_by_name(&v.name, &v.module_source)
                .is_some_and(|type_id| tt.is_reflect_eligible(type_id))
        })
        .map(|v| ReflectVariantTarget {
            name: v.name.clone(),
            type_params: v.type_params.clone(),
            cases: v
                .cases
                .iter()
                .map(|c| {
                    (
                        c.name.clone(),
                        c.index,
                        c.payload,
                        c.wire_name_override.clone(),
                    )
                })
                .collect(),
            span: v.span,
            wire_name_policy: v.wire_name_policy.clone(),
        })
        .collect()
}

/// Module-level types and method names resolved once from the compiler-item
/// registry and reused across every variant's `ReflectVariant` synthesis.
struct ReflectVariantSynthEnv {
    string_type: TypeId,
    member_struct_name: String,
    member_struct_module: ModuleSource,
    type_name_method: String,
    discriminant_method: String,
    cases_method: String,
    case_style_type: TypeId,
    wire_name_policy_method: String,
}

impl ReflectVariantSynthEnv {
    fn resolve(tt: &mut TypeTable) -> Self {
        let string_type = tt.make_compiler_struct(CompilerItem::String);
        let case_style_type = tt.make_compiler_enum(CompilerItem::CaseStyle);
        let items = tt.compiler_items();
        let (member_struct_module, member_struct_name) = {
            let (m, n) = items.require_struct(CompilerItem::ReflectVariantCase);
            (m.clone(), n.to_string())
        };
        Self {
            string_type,
            member_struct_name,
            member_struct_module,
            type_name_method: items
                .method_name(CompilerItem::ReflectVariantTypeName)
                .to_string(),
            discriminant_method: items
                .method_name(CompilerItem::ReflectVariantDiscriminant)
                .to_string(),
            cases_method: items
                .method_name(CompilerItem::ReflectVariantMembers)
                .to_string(),
            case_style_type,
            wire_name_policy_method: items
                .method_name(CompilerItem::ReflectVariantWireNamePolicy)
                .to_string(),
        }
    }
}

/// `ReflectVariant`'s payload-pack associated type (`type CasePayloads`): the
/// per-case payloads, with `()` for a unit case. Sealed and compiler-defined,
/// like [`REFLECT_FIELD_TYPES_ASSOC`].
pub(crate) const REFLECT_CASE_PAYLOADS_ASSOC: &str = "CasePayloads";

/// Synthesize one variant's `type_name()`, `discriminant(&self)`, and `cases()`
/// methods, plus the per-payload-type `Case` `extract` / `construct` helpers its
/// members dispatch to.
fn generate_variant_reflect_methods(
    type_table: &RefCell<TypeTable>,
    env: &ReflectVariantSynthEnv,
    module_source: &ModuleSource,
    variant_trait_name: &str,
    target: &ReflectVariantTarget,
) -> Vec<TirFunction> {
    let span = target.span;

    let type_name_fn = generate_type_name_fn(
        module_source,
        &target.name,
        env.string_type,
        variant_trait_name,
        &env.type_name_method,
        span,
    );

    let is_generic = !target.type_params.is_empty();

    let (variant_type, ref_variant_type, member_types, members_tuple_type) = {
        let mut tt = type_table.borrow_mut();
        let variant_type = if is_generic {
            let param_ids = make_type_param_ids(&target.type_params, &mut tt);
            tt.make_generic_instance(target.name.clone(), module_source.clone(), param_ids)
        } else {
            tt.make_variant(target.name.clone(), module_source.clone())
        };
        let ref_variant_type = tt.make_ref(variant_type);
        let member_types: Vec<TypeId> = target
            .cases
            .iter()
            .map(|(_, _, payload, _)| {
                tt.make_generic_instance(
                    env.member_struct_name.clone(),
                    env.member_struct_module.clone(),
                    vec![variant_type, *payload],
                )
            })
            .collect();
        let members_tuple_type = tt.make_tuple(member_types.clone());
        let payloads_tuple_type =
            tt.make_tuple(target.cases.iter().map(|(_, _, p, _)| *p).collect());
        register_reflect_assoc_types(
            &mut tt,
            variant_type,
            CompilerItem::ReflectVariant,
            is_generic,
            &[
                (REFLECT_CASE_PAYLOADS_ASSOC, payloads_tuple_type),
                (REFLECT_MEMBERS_ASSOC, members_tuple_type),
            ],
        );
        (
            variant_type,
            ref_variant_type,
            member_types,
            members_tuple_type,
        )
    };

    let discriminant_fn = generate_variant_discriminant_fn(
        module_source,
        &target.name,
        ref_variant_type,
        variant_type,
        variant_trait_name,
        &env.discriminant_method,
        span,
    );
    let cases_fn = generate_variant_cases_fn(
        module_source,
        type_table,
        env,
        variant_trait_name,
        target,
        &member_types,
        members_tuple_type,
        span,
    );

    let wire_name_policy_fn = generate_wire_name_policy_fn(
        module_source,
        &target.name,
        env.case_style_type,
        &target.wire_name_policy,
        variant_trait_name,
        &env.wire_name_policy_method,
        span,
    );

    let mut functions = vec![type_name_fn, discriminant_fn, cases_fn, wire_name_policy_fn];
    for f in &mut functions {
        f.impl_type_params.clone_from(&target.type_params);
    }
    // A generic variant's bridges are keyed by concrete payload types, so
    // `synthesize_monomorphized_reflect_bridges` mints them per instantiation.
    if !is_generic {
        functions.extend(generate_case_bridge_helpers(
            type_table,
            &target.cases,
            variant_type,
            ref_variant_type,
            span,
        ));
    }
    functions
}

/// Build `Variant^ReflectVariant::members()` as
/// `return [Case { index: 0, case_name: "…", wire_override: …, is_unit: … }, …];`
/// — one fat member per case, each typed `Case<Variant, P_k>` and carrying the
/// `Member` metadata alongside the payload bridge.
fn generate_variant_cases_fn(
    module_source: &ModuleSource,
    type_table: &RefCell<TypeTable>,
    env: &ReflectVariantSynthEnv,
    variant_trait_name: &str,
    target: &ReflectVariantTarget,
    member_types: &[TypeId],
    members_tuple_type: TypeId,
    span: Span,
) -> TirFunction {
    let method_info = trait_method_info(
        module_source,
        &target.name,
        variant_trait_name,
        &env.cases_method,
    );
    let qualified_name = method_info.to_mangled_name();

    let option_string_type = type_table.borrow_mut().make_option(env.string_type);

    let elements = target
        .cases
        .iter()
        .zip(member_types)
        .map(
            |((case_name, index, payload, wire_name_override), member_type)| {
                let wire_override = {
                    let tt = type_table.borrow();
                    let items = tt.compiler_items();
                    match wire_name_override {
                        Some(rename) => crate::synthesis::common::option_some(
                            TirExpr::new(
                                TirExprKind::StringLiteral(rename.clone()),
                                env.string_type,
                                span,
                            ),
                            option_string_type,
                            items,
                        ),
                        None => crate::synthesis::common::option_none(option_string_type, items),
                    }
                };
                let case_fields = vec![
                    reflect_meta_int_field("index", u64::from(*index), TypeTable::I32, 0, span),
                    TirStructField {
                        name: "case_name".to_string(),
                        value: TirExpr::new(
                            TirExprKind::StringLiteral(case_name.clone()),
                            env.string_type,
                            span,
                        ),
                        field_index: 1,
                    },
                    TirStructField {
                        name: "wire_override".to_string(),
                        value: wire_override,
                        field_index: 2,
                    },
                    TirStructField {
                        name: "is_unit".to_string(),
                        value: TirExpr::new(
                            TirExprKind::BoolLiteral(*payload == TypeTable::UNIT),
                            TypeTable::BOOL,
                            span,
                        ),
                        field_index: 3,
                    },
                ];
                TirExpr::new(
                    TirExprKind::StructLiteral {
                        struct_type: *member_type,
                        struct_name: env.member_struct_name.clone(),
                        fields: case_fields,
                    },
                    *member_type,
                    span,
                )
            },
        )
        .collect();
    let tuple = TirExpr::new(
        TirExprKind::TupleLiteral { elements },
        members_tuple_type,
        span,
    );

    let body = TirBlock::new(
        vec![TirStmt::new(
            TirStmtKind::Return { value: Some(tuple) },
            span,
        )],
        span,
    );

    make_synthetic_method(
        qualified_name,
        method_info,
        vec![],
        members_tuple_type,
        body,
        vec![],
    )
}

/// Synthesize the `$case_extract$V$P` / `$case_construct$V$P` helpers for
/// every distinct payload type of `target`. `Case::<V, P>::extract` and
/// `::construct` bodies carry `builtin::variant_case_*` markers; lowering
/// rewrites each monomorphized marker to its helper (WEP 2026-06-13 §3e).
pub(super) fn generate_case_bridge_helpers(
    type_table: &RefCell<TypeTable>,
    cases: &[(String, u32, TypeId, Option<String>)],
    variant_type: TypeId,
    ref_variant_type: TypeId,
    span: Span,
) -> Vec<TirFunction> {
    // Both mangles must match the post-erasure call site: lowering reads the
    // subject and payload through the erasure redirect map, so a `flags` or
    // newtype spelled here would mint a name nothing calls.
    let mangled_variant = type_table.borrow().mangle_type_arg_erased(variant_type);

    let mut by_payload: crate::hashmap::IndexMap<String, (TypeId, Vec<(String, u32)>)> =
        crate::hashmap::IndexMap::default();
    for (case_name, index, payload, _) in cases {
        let mangled = type_table.borrow().mangle_type_arg_erased(*payload);
        by_payload
            .entry(mangled)
            .or_insert_with(|| (*payload, Vec::new()))
            .1
            .push((case_name.clone(), *index));
    }

    let mut helpers = Vec::new();
    for (mangled_payload, (payload_type, cases)) in &by_payload {
        helpers.push(generate_case_extract_helper(
            crate::name::case_extract_helper_name(&mangled_variant, mangled_payload),
            variant_type,
            ref_variant_type,
            *payload_type,
            cases,
            span,
        ));
        helpers.push(generate_case_construct_helper(
            crate::name::case_construct_helper_name(&mangled_variant, mangled_payload),
            variant_type,
            *payload_type,
            cases,
            span,
        ));
    }
    helpers
}

/// A call to `builtin::unreachable()` typed as `result_type`, the trap arm of
/// the case-bridge dispatch.
fn unreachable_call(result_type: TypeId, span: Span) -> TirExpr {
    TirExpr::new(
        TirExprKind::Call {
            func: FunctionRef {
                module_source: ModuleSource::builtin(),
                name: "unreachable".to_string(),
                monomorph_info: None,
                method_info: None,
            },
            type_args: vec![],
            args: vec![],
        },
        result_type,
        span,
    )
}

/// Dispatch `match index { k => <arm(k)>, _ => unreachable() }` over the
/// cases sharing one payload type.
fn case_index_dispatch(
    index_expr: TirExpr,
    cases: &[(String, u32)],
    arm_body: impl Fn(&str, u32) -> TirExpr,
    result_type: TypeId,
    span: Span,
) -> TirExpr {
    let mut arms: Vec<TirMatchArm> = cases
        .iter()
        .map(|(case_name, index)| TirMatchArm {
            pattern: TirPattern::Literal(TirLiteralPattern::I128(i128::from(*index))),
            guard: None,
            body: arm_body(case_name, *index),
            span,
        })
        .collect();
    arms.push(TirMatchArm {
        pattern: TirPattern::Wildcard,
        guard: None,
        body: unreachable_call(result_type, span),
        span,
    });
    TirExpr::new(
        TirExprKind::Match {
            expr: Box::new(index_expr),
            arms,
        },
        result_type,
        span,
    )
}

/// Build `$case_extract$V$P(v: &V, index: i32) -> P`:
/// trap unless `v`'s tag is `index`, then read the case's payload.
fn generate_case_extract_helper(
    helper_name: String,
    variant_type: TypeId,
    ref_variant_type: TypeId,
    payload_type: TypeId,
    cases: &[(String, u32)],
    span: Span,
) -> TirFunction {
    let deref_v = || deref_local(0, "v", ref_variant_type, variant_type, span);
    let index_local = || local_expr(1, "index", TypeTable::I32, span);

    let tag = TirExpr::new(
        TirExprKind::VariantTag {
            expr: Box::new(deref_v()),
        },
        TypeTable::I32,
        span,
    );
    let guard = TirStmt::new(
        TirStmtKind::Expr(TirExpr::new(
            TirExprKind::If {
                condition: Box::new(TirExpr::new(
                    TirExprKind::Binary {
                        left: Box::new(tag),
                        op: TirBinaryOp::NotEq,
                        right: Box::new(index_local()),
                    },
                    TypeTable::BOOL,
                    span,
                )),
                then_branch: TirBlock::new(
                    vec![TirStmt::new(
                        TirStmtKind::Expr(unreachable_call(TypeTable::UNIT, span)),
                        span,
                    )],
                    span,
                ),
                else_branch: None,
            },
            TypeTable::UNIT,
            span,
        )),
        span,
    );

    let dispatch = case_index_dispatch(
        index_local(),
        cases,
        |_, index| {
            TirExpr::new(
                TirExprKind::VariantPayload {
                    expr: Box::new(deref_v()),
                    case_index: index,
                    payload_type,
                },
                payload_type,
                span,
            )
        },
        payload_type,
        span,
    );

    let body = TirBlock::new(
        vec![
            guard,
            TirStmt::new(
                TirStmtKind::Return {
                    value: Some(dispatch),
                },
                span,
            ),
        ],
        span,
    );

    make_synthetic_free_function(
        helper_name,
        vec![
            TirParam {
                name: "v".to_string(),
                type_id: ref_variant_type,
                local_index: 0,
                is_mut: false,
                is_mut_ref: false,
                span,
            },
            TirParam {
                name: "index".to_string(),
                type_id: TypeTable::I32,
                local_index: 1,
                is_mut: false,
                is_mut_ref: false,
                span,
            },
        ],
        payload_type,
        body,
        vec![
            param_local("v", ref_variant_type, false),
            param_local("index", TypeTable::I32, false),
        ],
    )
}

/// Build `$case_construct$V$P(payload: P, index: i32) -> V`:
/// construct case `index` around `payload`.
fn generate_case_construct_helper(
    helper_name: String,
    variant_type: TypeId,
    payload_type: TypeId,
    cases: &[(String, u32)],
    span: Span,
) -> TirFunction {
    let is_unit_payload = payload_type == TypeTable::UNIT;
    let dispatch = case_index_dispatch(
        local_expr(1, "index", TypeTable::I32, span),
        cases,
        |case_name, index| {
            TirExpr::new(
                TirExprKind::VariantConstruct {
                    variant_type,
                    case_index: index,
                    case_name: case_name.to_string(),
                    payload: if is_unit_payload {
                        None
                    } else {
                        Some(Box::new(local_expr(0, "payload", payload_type, span)))
                    },
                },
                variant_type,
                span,
            )
        },
        variant_type,
        span,
    );

    let body = TirBlock::new(
        vec![TirStmt::new(
            TirStmtKind::Return {
                value: Some(dispatch),
            },
            span,
        )],
        span,
    );

    make_synthetic_free_function(
        helper_name,
        vec![
            TirParam {
                name: "payload".to_string(),
                type_id: payload_type,
                local_index: 0,
                is_mut: false,
                is_mut_ref: false,
                span,
            },
            TirParam {
                name: "index".to_string(),
                type_id: TypeTable::I32,
                local_index: 1,
                is_mut: false,
                is_mut_ref: false,
                span,
            },
        ],
        variant_type,
        body,
        vec![
            param_local("payload", payload_type, false),
            param_local("index", TypeTable::I32, false),
        ],
    )
}

/// Build `Variant^ReflectVariant::discriminant(&self) -> i32` as
/// `return <tag of *self>;`.
fn generate_variant_discriminant_fn(
    module_source: &ModuleSource,
    variant_name: &str,
    ref_variant_type: TypeId,
    variant_type: TypeId,
    variant_trait_name: &str,
    discriminant_method: &str,
    span: Span,
) -> TirFunction {
    let method_info = trait_method_info(
        module_source,
        variant_name,
        variant_trait_name,
        discriminant_method,
    );
    let qualified_name = method_info.to_mangled_name();
    let mut function = make_synthetic_method(
        qualified_name,
        method_info,
        vec![self_param(ref_variant_type, span)],
        TypeTable::I32,
        variant_tag_body(ref_variant_type, variant_type, span),
        vec![param_local("self", ref_variant_type, false)],
    );
    function.locals = vec![param_local("self", ref_variant_type, false)];
    function
}

/// `return <tag of *self>;` — the body every `discriminant` shares.
fn variant_tag_body(ref_variant_type: TypeId, variant_type: TypeId, span: Span) -> TirBlock {
    let tag = TirExpr::new(
        TirExprKind::VariantTag {
            expr: Box::new(deref_local(0, "self", ref_variant_type, variant_type, span)),
        },
        TypeTable::I32,
        span,
    );
    TirBlock::new(
        vec![TirStmt::new(TirStmtKind::Return { value: Some(tag) }, span)],
        span,
    )
}

/// The `discriminant` of one instantiated generic variant, as a free function
/// under the tag-helper name lowering builds from the instance
/// (`$variant_tag$<mangle>`). Not a method: the method-name machinery rejects
/// type arguments in a base struct name.
pub(super) fn generate_variant_instance_discriminant_fn(
    qualified_name: String,
    ref_variant_type: TypeId,
    variant_type: TypeId,
    span: Span,
) -> TirFunction {
    crate::synthesis::common::make_synthetic_free_function(
        qualified_name,
        vec![self_param(ref_variant_type, span)],
        TypeTable::I32,
        variant_tag_body(ref_variant_type, variant_type, span),
        vec![param_local("self", ref_variant_type, false)],
    )
}

/// Generate the `ReflectEnum` members for each requested enum
/// (WEP 2026-06-13 §3b): `type_name()`, `discriminant(&self)`,
/// `from_discriminant(disc)`, and `members()`.
pub fn synthesize_reflect_enum(project: &mut Package) {
    synthesize_reflect_kind(
        project,
        CompilerItem::ReflectEnum,
        generate_enum_reflect_impls,
    );
}

/// Synthesize the `ReflectEnum` impl of every requested enum in `module`.
fn generate_enum_reflect_impls(
    module: &mut TirModule,
    ctx: &mut SynthesisCtx<'_, '_, '_>,
    enum_trait_name: &str,
) {
    if module.enums.is_empty() {
        return;
    }

    let targets: Vec<ReflectEnumTarget> = module
        .enums
        .iter()
        .filter(|e| e.type_params.is_empty())
        .map(|e| ReflectEnumTarget {
            name: e.name.clone(),
            cases: e
                .cases
                .iter()
                .map(|c| (c.name.clone(), c.index, c.wire_name_override.clone()))
                .collect(),
            span: e.span,
            wire_name_policy: e.wire_name_policy.clone(),
        })
        .collect();
    if targets.is_empty() {
        return;
    }

    let module_source = module.module_source.clone();
    let env = ReflectEnumSynthEnv::resolve(&mut module.type_table.borrow_mut());

    let mut generated = Vec::new();
    for target in &targets {
        let methods = generate_enum_reflect_methods(
            &module.type_table,
            &env,
            &module_source,
            enum_trait_name,
            target,
        );
        generated.extend(methods.into_iter().map(|f| Rc::new(RefCell::new(f))));
        ctx.record_impl(&target.name, enum_trait_name);
    }

    module.functions.extend(generated);
}

/// An enum selected for `ReflectEnum` synthesis.
struct ReflectEnumTarget {
    name: String,
    /// Per-case `(name, index, #[wire(name)])`; a case's discriminant is
    /// its index.
    cases: Vec<(String, u32, Option<String>)>,
    span: Span,
    wire_name_policy: Option<String>,
}

/// Module-level types and method names resolved once from the compiler-item
/// registry and reused across every enum's `ReflectEnum` synthesis.
struct ReflectEnumSynthEnv {
    string_type: TypeId,
    member_struct_name: String,
    member_struct_module: ModuleSource,
    type_name_method: String,
    discriminant_method: String,
    from_discriminant_method: String,
    members_method: String,
    case_style_type: TypeId,
    wire_name_policy_method: String,
}

impl ReflectEnumSynthEnv {
    fn resolve(tt: &mut TypeTable) -> Self {
        let string_type = tt.make_compiler_struct(CompilerItem::String);
        let case_style_type = tt.make_compiler_enum(CompilerItem::CaseStyle);
        let items = tt.compiler_items();
        let (member_struct_module, member_struct_name) = {
            let (m, n) = items.require_struct(CompilerItem::ReflectEnumCase);
            (m.clone(), n.to_string())
        };
        Self {
            string_type,
            member_struct_name,
            member_struct_module,
            type_name_method: items
                .method_name(CompilerItem::ReflectEnumTypeName)
                .to_string(),
            discriminant_method: items
                .method_name(CompilerItem::ReflectEnumDiscriminant)
                .to_string(),
            from_discriminant_method: items
                .method_name(CompilerItem::ReflectEnumFromDiscriminant)
                .to_string(),
            members_method: items
                .method_name(CompilerItem::ReflectEnumMembers)
                .to_string(),
            case_style_type,
            wire_name_policy_method: items
                .method_name(CompilerItem::ReflectEnumWireNamePolicy)
                .to_string(),
        }
    }
}

/// Synthesize one enum's `type_name()`, `discriminant(&self)`,
/// `from_discriminant(disc)`, and `members()` methods.
fn generate_enum_reflect_methods(
    type_table: &RefCell<TypeTable>,
    env: &ReflectEnumSynthEnv,
    module_source: &ModuleSource,
    enum_trait_name: &str,
    target: &ReflectEnumTarget,
) -> Vec<TirFunction> {
    let span = target.span;

    let type_name_fn = generate_type_name_fn(
        module_source,
        &target.name,
        env.string_type,
        enum_trait_name,
        &env.type_name_method,
        span,
    );

    let (enum_type, ref_enum_type, option_enum_type, member_type, members_tuple_type) = {
        let mut tt = type_table.borrow_mut();
        let enum_type = tt.make_enum(target.name.clone(), module_source.clone());
        let ref_enum_type = tt.make_ref(enum_type);
        let option_enum_type = tt.make_option(enum_type);
        let member_type = tt.make_generic_instance(
            env.member_struct_name.clone(),
            env.member_struct_module.clone(),
            vec![enum_type],
        );
        let members_tuple_type =
            tt.make_tuple(std::iter::repeat_n(member_type, target.cases.len()).collect());
        let reflect_enum = tt
            .compiler_items()
            .trait_name(CompilerItem::ReflectEnum)
            .to_string();
        tt.register_assoc_type_resolution(
            enum_type,
            reflect_enum,
            REFLECT_MEMBERS_ASSOC.to_string(),
            members_tuple_type,
        );
        (
            enum_type,
            ref_enum_type,
            option_enum_type,
            member_type,
            members_tuple_type,
        )
    };

    let discriminant_fn = generate_enum_discriminant_fn(
        module_source,
        env,
        enum_trait_name,
        target,
        enum_type,
        ref_enum_type,
        span,
    );
    let from_discriminant_fn = generate_enum_from_discriminant_fn(
        module_source,
        type_table,
        env,
        enum_trait_name,
        target,
        enum_type,
        option_enum_type,
        span,
    );
    let members_fn = generate_enum_members_fn(
        module_source,
        type_table,
        env,
        enum_trait_name,
        target,
        enum_type,
        member_type,
        members_tuple_type,
        span,
    );

    let wire_name_policy_fn = generate_wire_name_policy_fn(
        module_source,
        &target.name,
        env.case_style_type,
        &target.wire_name_policy,
        enum_trait_name,
        &env.wire_name_policy_method,
        span,
    );

    vec![
        type_name_fn,
        discriminant_fn,
        from_discriminant_fn,
        members_fn,
        wire_name_policy_fn,
    ]
}

/// Build `Enum^ReflectEnum::members() -> Self::Members` as
/// `return [EnumCase { discriminant: k, value: Enum::Case_k, case_name: "…",
/// wire_override: … }, …];` — one `EnumCase<Enum>` per case, packed into the
/// homogeneous `Members` tuple.
fn generate_enum_members_fn(
    module_source: &ModuleSource,
    type_table: &RefCell<TypeTable>,
    env: &ReflectEnumSynthEnv,
    enum_trait_name: &str,
    target: &ReflectEnumTarget,
    enum_type: TypeId,
    member_type: TypeId,
    members_tuple_type: TypeId,
    span: Span,
) -> TirFunction {
    let method_info = trait_method_info(
        module_source,
        &target.name,
        enum_trait_name,
        &env.members_method,
    );

    let option_string_type = type_table.borrow_mut().make_option(env.string_type);

    let rows = target
        .cases
        .iter()
        .map(|(case_name, index, wire_name_override)| {
            let wire_override = {
                let tt = type_table.borrow();
                let items = tt.compiler_items();
                match wire_name_override {
                    Some(rename) => crate::synthesis::common::option_some(
                        TirExpr::new(
                            TirExprKind::StringLiteral(rename.clone()),
                            env.string_type,
                            span,
                        ),
                        option_string_type,
                        items,
                    ),
                    None => crate::synthesis::common::option_none(option_string_type, items),
                }
            };
            let value = TirExpr::new(
                TirExprKind::EnumConstruct {
                    enum_type,
                    case_index: *index,
                    case_name: case_name.clone(),
                },
                enum_type,
                span,
            );
            vec![
                reflect_meta_int_field("discriminant", u64::from(*index), TypeTable::I32, 0, span),
                TirStructField {
                    name: "value".to_string(),
                    value,
                    field_index: 1,
                },
                TirStructField {
                    name: "case_name".to_string(),
                    value: TirExpr::new(
                        TirExprKind::StringLiteral(case_name.clone()),
                        env.string_type,
                        span,
                    ),
                    field_index: 2,
                },
                TirStructField {
                    name: "wire_override".to_string(),
                    value: wire_override,
                    field_index: 3,
                },
            ]
        })
        .collect();

    generate_reflect_member_tuple_fn(
        method_info,
        &env.member_struct_name,
        member_type,
        members_tuple_type,
        rows,
        span,
    )
}

/// Build `Enum^ReflectEnum::discriminant(&self) -> i32` as `return *self as i32;`
/// — an enum value is its i32 discriminant, so a direct cast reads the tag (the
/// enum analog of `ReflectFlags::bits`'s `*self as u32`).
fn generate_enum_discriminant_fn(
    module_source: &ModuleSource,
    env: &ReflectEnumSynthEnv,
    enum_trait_name: &str,
    target: &ReflectEnumTarget,
    enum_type: TypeId,
    ref_enum_type: TypeId,
    span: Span,
) -> TirFunction {
    let method_info = trait_method_info(
        module_source,
        &target.name,
        enum_trait_name,
        &env.discriminant_method,
    );
    let qualified_name = method_info.to_mangled_name();

    let as_i32 = crate::synthesis::common::cast(
        deref_local(0, "self", ref_enum_type, enum_type, span),
        TypeTable::I32,
    );
    let body = TirBlock::new(
        vec![TirStmt::new(
            TirStmtKind::Return {
                value: Some(as_i32),
            },
            span,
        )],
        span,
    );

    make_synthetic_method(
        qualified_name,
        method_info,
        vec![self_param(ref_enum_type, span)],
        TypeTable::I32,
        body,
        vec![param_local("self", ref_enum_type, false)],
    )
}

/// Build `Enum^ReflectEnum::from_discriminant(disc: i32) -> Option<Enum>` as
/// an `if disc == k { return Some(Case_k); }` chain ending in `None`.
fn generate_enum_from_discriminant_fn(
    module_source: &ModuleSource,
    type_table: &RefCell<TypeTable>,
    env: &ReflectEnumSynthEnv,
    enum_trait_name: &str,
    target: &ReflectEnumTarget,
    enum_type: TypeId,
    option_enum_type: TypeId,
    span: Span,
) -> TirFunction {
    let method_info = trait_method_info(
        module_source,
        &target.name,
        enum_trait_name,
        &env.from_discriminant_method,
    );
    let qualified_name = method_info.to_mangled_name();

    let mut stmts = Vec::new();
    for (case_name, index, _) in &target.cases {
        let comparison = TirExpr::new(
            TirExprKind::Binary {
                op: TirBinaryOp::Eq,
                left: Box::new(local_expr(0, "disc", TypeTable::I32, span)),
                right: Box::new(TirExpr::new(
                    TirExprKind::IntLiteral {
                        value: u64::from(*index),
                        repr: index.to_string(),
                    },
                    TypeTable::I32,
                    span,
                )),
            },
            TypeTable::BOOL,
            span,
        );
        let case_value = TirExpr::new(
            TirExprKind::EnumConstruct {
                enum_type,
                case_index: *index,
                case_name: case_name.clone(),
            },
            enum_type,
            span,
        );
        let some = crate::synthesis::common::option_some(
            case_value,
            option_enum_type,
            type_table.borrow().compiler_items(),
        );
        stmts.push(TirStmt::new(
            TirStmtKind::If {
                condition: comparison,
                then_block: TirBlock::new(
                    vec![TirStmt::new(
                        TirStmtKind::Return { value: Some(some) },
                        span,
                    )],
                    span,
                ),
                else_block: None,
            },
            span,
        ));
    }
    let none = crate::synthesis::common::option_none(
        option_enum_type,
        type_table.borrow().compiler_items(),
    );
    stmts.push(TirStmt::new(
        TirStmtKind::Return { value: Some(none) },
        span,
    ));

    let disc_param = TirParam {
        name: "disc".to_string(),
        type_id: TypeTable::I32,
        local_index: 0,
        is_mut: false,
        is_mut_ref: false,
        span,
    };
    make_synthetic_method(
        qualified_name,
        method_info,
        vec![disc_param],
        option_enum_type,
        TirBlock::new(stmts, span),
        vec![param_local("disc", TypeTable::I32, false)],
    )
}

/// Generate the `ReflectFlags` members for each requested flags type
/// (WEP 2026-06-13 §3c): `type_name()`, `bits(&self)`, `from_bits(raw)` — the
/// u64-normalized bit bridge — and `members()`.
pub fn synthesize_reflect_flags(project: &mut Package) {
    synthesize_reflect_kind(
        project,
        CompilerItem::ReflectFlags,
        generate_flags_reflect_impls,
    );
}

/// Synthesize the `ReflectFlags` impl of every requested flags type in `module`.
fn generate_flags_reflect_impls(
    module: &mut TirModule,
    ctx: &mut SynthesisCtx<'_, '_, '_>,
    flags_trait_name: &str,
) {
    if module.flags.is_empty() {
        return;
    }

    let module_source = module.module_source.clone();
    let targets: Vec<ReflectFlagsTarget> = module
        .flags
        .iter()
        .filter_map(|f| {
            let flags_type = module
                .type_table
                .borrow()
                .find_flags_type(&f.name, &module_source)?;
            Some(ReflectFlagsTarget {
                name: f.name.clone(),
                flags_type,
                members: f
                    .members
                    .iter()
                    .map(|m| (m.name.clone(), m.bitmask))
                    .collect(),
                span: f.span,
                wire_name_policy: f.wire_name_policy.clone(),
            })
        })
        .collect();
    if targets.is_empty() {
        return;
    }

    let env = ReflectFlagsSynthEnv::resolve(&mut module.type_table.borrow_mut());

    let mut generated = Vec::new();
    for target in &targets {
        let methods = generate_flags_reflect_methods(
            &module_source,
            &module.type_table,
            &env,
            flags_trait_name,
            target,
        );
        generated.extend(methods.into_iter().map(|f| Rc::new(RefCell::new(f))));
        ctx.record_impl(&target.name, flags_trait_name);
    }

    module.functions.extend(generated);
}

/// A flags type selected for `ReflectFlags` synthesis.
struct ReflectFlagsTarget {
    name: String,
    flags_type: TypeId,
    /// Per-member `(name, bitmask)`.
    members: Vec<(String, u32)>,
    span: Span,
    wire_name_policy: Option<String>,
}

/// Module-level types and method names resolved once from the compiler-item
/// registry and reused across every flags type's `ReflectFlags` synthesis.
struct ReflectFlagsSynthEnv {
    string_type: TypeId,
    member_struct_name: String,
    member_struct_module: ModuleSource,
    type_name_method: String,
    bits_method: String,
    from_bits_method: String,
    members_method: String,
    case_style_type: TypeId,
    wire_name_policy_method: String,
}

impl ReflectFlagsSynthEnv {
    fn resolve(tt: &mut TypeTable) -> Self {
        let string_type = tt.make_compiler_struct(CompilerItem::String);
        let case_style_type = tt.make_compiler_enum(CompilerItem::CaseStyle);
        let items = tt.compiler_items();
        let (member_struct_module, member_struct_name) = {
            let (m, n) = items.require_struct(CompilerItem::ReflectFlagsBit);
            (m.clone(), n.to_string())
        };
        Self {
            string_type,
            member_struct_name,
            member_struct_module,
            type_name_method: items
                .method_name(CompilerItem::ReflectFlagsTypeName)
                .to_string(),
            bits_method: items
                .method_name(CompilerItem::ReflectFlagsBits)
                .to_string(),
            from_bits_method: items
                .method_name(CompilerItem::ReflectFlagsFromBits)
                .to_string(),
            members_method: items
                .method_name(CompilerItem::ReflectFlagsMembers)
                .to_string(),
            case_style_type,
            wire_name_policy_method: items
                .method_name(CompilerItem::ReflectFlagsWireNamePolicy)
                .to_string(),
        }
    }
}

/// Synthesize one flags type's `type_name()`, `bits(&self)`, `from_bits(raw)`,
/// and `members()` methods.
fn generate_flags_reflect_methods(
    module_source: &ModuleSource,
    type_table: &RefCell<TypeTable>,
    env: &ReflectFlagsSynthEnv,
    flags_trait_name: &str,
    target: &ReflectFlagsTarget,
) -> Vec<TirFunction> {
    let span = target.span;

    let type_name_fn = generate_type_name_fn(
        module_source,
        &target.name,
        env.string_type,
        flags_trait_name,
        &env.type_name_method,
        span,
    );

    let (ref_flags_type, option_flags_type, member_type, members_tuple_type) = {
        let mut tt = type_table.borrow_mut();
        let ref_flags_type = tt.make_ref(target.flags_type);
        let option_flags_type = tt.make_option(target.flags_type);
        let member_type = tt.make_generic_instance(
            env.member_struct_name.clone(),
            env.member_struct_module.clone(),
            vec![target.flags_type],
        );
        let members_tuple_type =
            tt.make_tuple(std::iter::repeat_n(member_type, target.members.len()).collect());
        let reflect_flags = tt
            .compiler_items()
            .trait_name(CompilerItem::ReflectFlags)
            .to_string();
        tt.register_assoc_type_resolution(
            target.flags_type,
            reflect_flags,
            REFLECT_MEMBERS_ASSOC.to_string(),
            members_tuple_type,
        );
        (
            ref_flags_type,
            option_flags_type,
            member_type,
            members_tuple_type,
        )
    };

    let bits_fn = generate_flags_bits_fn(
        module_source,
        env,
        flags_trait_name,
        target,
        ref_flags_type,
        span,
    );
    let from_bits_fn = generate_flags_from_bits_fn(
        module_source,
        type_table,
        env,
        flags_trait_name,
        target,
        option_flags_type,
        span,
    );
    let members_fn = generate_flags_members_fn(
        module_source,
        env,
        flags_trait_name,
        target,
        member_type,
        members_tuple_type,
        span,
    );

    let wire_name_policy_fn = generate_wire_name_policy_fn(
        module_source,
        &target.name,
        env.case_style_type,
        &target.wire_name_policy,
        flags_trait_name,
        &env.wire_name_policy_method,
        span,
    );

    vec![
        type_name_fn,
        bits_fn,
        from_bits_fn,
        members_fn,
        wire_name_policy_fn,
    ]
}

/// Build `Flags^ReflectFlags::members() -> Self::Members` as
/// `return [FlagsBit { bit: b, value: (b as Flags), member_name: "…" }, …];` —
/// one `FlagsBit<Flags>` per member, packed into the homogeneous `Members`
/// tuple.
fn generate_flags_members_fn(
    module_source: &ModuleSource,
    env: &ReflectFlagsSynthEnv,
    flags_trait_name: &str,
    target: &ReflectFlagsTarget,
    member_type: TypeId,
    members_tuple_type: TypeId,
    span: Span,
) -> TirFunction {
    let method_info = trait_method_info(
        module_source,
        &target.name,
        flags_trait_name,
        &env.members_method,
    );

    let rows = target
        .members
        .iter()
        .map(|(member_name, bitmask)| {
            let bit_u32 = TirExpr::new(
                TirExprKind::IntLiteral {
                    value: u64::from(*bitmask),
                    repr: bitmask.to_string(),
                },
                TypeTable::U32,
                span,
            );
            let value = crate::synthesis::common::cast(bit_u32, target.flags_type);
            vec![
                reflect_meta_int_field("bit", u64::from(*bitmask), TypeTable::U64, 0, span),
                TirStructField {
                    name: "value".to_string(),
                    value,
                    field_index: 1,
                },
                TirStructField {
                    name: "member_name".to_string(),
                    value: TirExpr::new(
                        TirExprKind::StringLiteral(member_name.clone()),
                        env.string_type,
                        span,
                    ),
                    field_index: 2,
                },
            ]
        })
        .collect();

    generate_reflect_member_tuple_fn(
        method_info,
        &env.member_struct_name,
        member_type,
        members_tuple_type,
        rows,
        span,
    )
}

/// Build `Flags^ReflectFlags::bits(&self) -> u64` as
/// `return (*self as u32) as u64;` — the widening is lossless.
fn generate_flags_bits_fn(
    module_source: &ModuleSource,
    env: &ReflectFlagsSynthEnv,
    flags_trait_name: &str,
    target: &ReflectFlagsTarget,
    ref_flags_type: TypeId,
    span: Span,
) -> TirFunction {
    let method_info = trait_method_info(
        module_source,
        &target.name,
        flags_trait_name,
        &env.bits_method,
    );
    let qualified_name = method_info.to_mangled_name();

    let as_u32 = crate::synthesis::common::cast(
        deref_local(0, "self", ref_flags_type, target.flags_type, span),
        TypeTable::U32,
    );
    let as_u64 = crate::synthesis::common::cast(as_u32, TypeTable::U64);
    let body = TirBlock::new(
        vec![TirStmt::new(
            TirStmtKind::Return {
                value: Some(as_u64),
            },
            span,
        )],
        span,
    );

    make_synthetic_method(
        qualified_name,
        method_info,
        vec![self_param(ref_flags_type, span)],
        TypeTable::U64,
        body,
        vec![param_local("self", ref_flags_type, false)],
    )
}

/// Build `Flags^ReflectFlags::from_bits(raw: u64) -> Option<Flags>` as
/// `if (raw & VALID) != raw { return None; } return Some((raw as u32) as F);`
/// — unknown bits are rejected (CM semantics).
fn generate_flags_from_bits_fn(
    module_source: &ModuleSource,
    type_table: &RefCell<TypeTable>,
    env: &ReflectFlagsSynthEnv,
    flags_trait_name: &str,
    target: &ReflectFlagsTarget,
    option_flags_type: TypeId,
    span: Span,
) -> TirFunction {
    let method_info = trait_method_info(
        module_source,
        &target.name,
        flags_trait_name,
        &env.from_bits_method,
    );
    let qualified_name = method_info.to_mangled_name();

    let valid_mask: u64 = target
        .members
        .iter()
        .fold(0u64, |acc, (_, bitmask)| acc | u64::from(*bitmask));
    let u64_literal = |value: u64| {
        TirExpr::new(
            TirExprKind::IntLiteral {
                value,
                repr: value.to_string(),
            },
            TypeTable::U64,
            span,
        )
    };

    let masked = TirExpr::new(
        TirExprKind::Binary {
            op: TirBinaryOp::BitAnd,
            left: Box::new(local_expr(0, "raw", TypeTable::U64, span)),
            right: Box::new(u64_literal(valid_mask)),
        },
        TypeTable::U64,
        span,
    );
    let has_unknown_bits = TirExpr::new(
        TirExprKind::Binary {
            op: TirBinaryOp::NotEq,
            left: Box::new(masked),
            right: Box::new(local_expr(0, "raw", TypeTable::U64, span)),
        },
        TypeTable::BOOL,
        span,
    );
    let none = crate::synthesis::common::option_none(
        option_flags_type,
        type_table.borrow().compiler_items(),
    );
    let reject = TirStmt::new(
        TirStmtKind::If {
            condition: has_unknown_bits,
            then_block: TirBlock::new(
                vec![TirStmt::new(
                    TirStmtKind::Return { value: Some(none) },
                    span,
                )],
                span,
            ),
            else_block: None,
        },
        span,
    );

    let as_u32 =
        crate::synthesis::common::cast(local_expr(0, "raw", TypeTable::U64, span), TypeTable::U32);
    let as_flags = crate::synthesis::common::cast(as_u32, target.flags_type);
    let some = crate::synthesis::common::option_some(
        as_flags,
        option_flags_type,
        type_table.borrow().compiler_items(),
    );
    let accept = TirStmt::new(TirStmtKind::Return { value: Some(some) }, span);

    let raw_param = TirParam {
        name: "raw".to_string(),
        type_id: TypeTable::U64,
        local_index: 0,
        is_mut: false,
        is_mut_ref: false,
        span,
    };
    make_synthetic_method(
        qualified_name,
        method_info,
        vec![raw_param],
        option_flags_type,
        TirBlock::new(vec![reject, accept], span),
        vec![param_local("raw", TypeTable::U64, false)],
    )
}

/// Threading of trait-impl knowledge through the synthesis sub-passes.
///
/// `trait_env` exposes the AST-layer (and any prior synthesis-layer) impls
/// already known to the project; `pending` is the in-progress dedup set
/// that grows as each sub-pass adds new impls. Together they let a
/// sub-pass answer "is `impl <trait> for <type>` already in the project?"
/// without re-scanning TIR per call.
pub(crate) struct SynthesisCtx<'env, 'pend, 'req> {
    pub(crate) trait_env: &'env TraitEnv,
    /// In-progress dedup of `(type_name, module, trait_name)` triples. Module
    /// is part of the key so that two same-name structs from different
    /// modules each get their own auto-derived impl — without the module
    /// component, the second derivation would be silently skipped and the
    /// receiver type from the second module would dispatch to the first
    /// module's impl.
    pub(crate) pending: &'pend mut IndexSet<(String, ModuleSource, String)>,
    /// `(type_name, module, trait_name)` triples that a real `T: Eq` /
    /// `T: Ord` bound or explicit marker actually demanded (WEP
    /// 2026-06-25-trait-derivation), snapshotted from
    /// `TypeTable::bound_driven_synth_requests` before this pass starts.
    /// Gates `generate_enum_trait_impls` / `generate_struct_eq_ord_impls` /
    /// `generate_variant_eq_impls` — an impl is emitted only for a pair
    /// recorded here, not for every declared type. Default / Inspect /
    /// Display and their `Alt` siblings stay unconditional.
    pub(crate) requested: &'req IndexSet<(String, ModuleSource, String)>,
    /// Module currently being synthesised. Auto-derived impls live in this
    /// module by convention.
    pub(crate) module: ModuleSource,
    /// Snapshot of every `core:prelude/{traits,format}` symbol this pass
    /// touches, resolved once through the [`CompilerItem`] registry.
    /// Threaded through `SynthesisCtx` so every sub-pass (Inspect /
    /// `InspectAlt` / Display / `DisplayAlt` fallbacks, plus the helpers that
    /// build trait method names) reads the registered trait / struct names
    /// instead of hard-coding `"Inspect"` / `"Formatter"` / etc.
    pub(crate) names: &'env TraitsStdlibNames,
}

impl SynthesisCtx<'_, '_, '_> {
    /// `true` when an impl of `trait_name` for `<type_name>` is already known
    /// to the project — either user-written (in the AST layer of `TraitEnv`,
    /// regardless of which module it lives in) or generated earlier in this
    /// synthesis pass for the *current* module.
    ///
    /// The two halves are deliberately scoped differently:
    ///
    /// - The AST-layer check is module-agnostic. A user-written
    ///   `impl Display for String` in `core:prelude/format` must suppress
    ///   `synthesize_traits`'s DisplayAlt-delegates-to-Display fallback even
    ///   when this pass is currently synthesising `core:prelude/string`
    ///   (String's defining module). Restricting the check to
    ///   `self.module` would silently shadow the user's impl with the
    ///   auto-derived fallback the synthesised layer later wins via the
    ///   `type_module` hint at the call site.
    /// - The in-pass `pending` check stays module-scoped so two same-name
    ///   receiver types in different modules (e.g. `struct Widget` in
    ///   module A and module B) each still get their own auto-derived
    ///   impl. Without the module component the second derivation would
    ///   be silently skipped.
    pub(crate) fn has_impl(&self, type_name: &str, trait_name: &str) -> bool {
        // Module-agnostic AST-layer check: any user-written impl, anywhere
        // in the project, counts. During synthesis the synthesised layer of
        // `TraitEnv` is empty (it is rebuilt by `collect_synthesised_impls`
        // *after* this pass), so `impl_module_for` with no hint reduces to
        // the AST layer.
        if self
            .trait_env
            .impl_module_for(type_name, trait_name, None)
            .is_some()
        {
            return true;
        }
        self.pending.contains(&(
            type_name.to_string(),
            self.module.clone(),
            trait_name.to_string(),
        ))
    }

    /// Note that this synthesis pass added `impl <trait_name> for <type_name>`
    /// in the current module. Used for in-pass dedup only; the canonical
    /// synthesis layer is rebuilt by `collect_synthesised_impls` after
    /// `synthesize_traits` returns.
    pub(crate) fn record_impl(&mut self, type_name: &str, trait_name: &str) {
        self.pending.insert((
            type_name.to_string(),
            self.module.clone(),
            trait_name.to_string(),
        ));
    }

    /// `true` when some `T: <trait_name>` bound (or an explicit marker) in
    /// the project actually demanded `impl <trait_name> for <type_name>` in
    /// the current module — see [`Self::requested`]. Only consulted for the
    /// `Eq` / `Ord` sub-passes; the other auto-derives stay unconditional.
    pub(crate) fn is_requested(&self, type_name: &str, trait_name: &str) -> bool {
        self.requested.contains(&(
            type_name.to_string(),
            self.module.clone(),
            trait_name.to_string(),
        ))
    }

    /// The receiver `type_name` indexes under in [`TraitEnv`]. A sub-pass names
    /// the declarations of the module it is synthesising, so that module is the
    /// declaring one; the key is built exactly as
    /// [`super::super::elaborator::trait_env::ImplTargetKey::receiver`] builds
    /// the definition side.
    fn receiver(&self, type_name: &str) -> Receiver {
        Receiver::Type(FqTypeName::of_head(&self.module, type_name))
    }

    /// `true` when this pass already emitted `<trait_name> for <type_name>`.
    fn pending_has(&self, type_name: &str, trait_name: &str) -> bool {
        self.pending.contains(&(
            type_name.to_string(),
            self.module.clone(),
            trait_name.to_string(),
        ))
    }

    /// `true` when a *methodful* impl already covers `<trait_name> for
    /// <type_name>` (within `scope`) or this pass emitted one. A body-less
    /// `impl Trait for Type;` marker does not count — it must not block the
    /// body it asks for.
    fn has_methodful_impl(&self, type_name: &str, trait_name: &str, scope: ImplScope) -> bool {
        let type_key = self.receiver(type_name);
        let real = match scope {
            ImplScope::CurrentModule => {
                self.trait_env
                    .has_methodful_impl_by_receiver(&type_key, trait_name, &self.module)
            }
            ImplScope::AnyModule => self
                .trait_env
                .has_any_methodful_impl_by_receiver(&type_key, trait_name),
        };
        real || self.pending_has(type_name, trait_name)
    }

    /// Module-scoped methodful check, for the `Eq` / `Ord` / `Default`
    /// sub-passes.
    pub(crate) fn has_real_impl(&self, type_name: &str, trait_name: &str) -> bool {
        self.has_methodful_impl(type_name, trait_name, ImplScope::CurrentModule)
    }

    pub(crate) fn has_methodful_impl_anywhere(&self, type_name: &str, trait_name: &str) -> bool {
        self.has_methodful_impl(type_name, trait_name, ImplScope::AnyModule)
    }

    pub(crate) fn should_synthesize(&self, type_name: &str, trait_name: &str) -> bool {
        self.is_requested(type_name, trait_name) && !self.has_real_impl(type_name, trait_name)
    }
}

#[derive(Clone, Copy)]
enum ImplScope {
    CurrentModule,
    AnyModule,
}

/// The module declaring a shape's base (`Stream` for `Stream<u8>`), which names
/// its synthesized impls. `None` for a shape with no declaration to point at.
fn shape_declaring_module(resolved: &ResolvedType) -> Option<ModuleSource> {
    match resolved {
        ResolvedType::GenericInstance { module_source, .. }
        | ResolvedType::GenericResource { module_source, .. } => Some(module_source.clone()),
        _ => None,
    }
}

/// Resolve type parameter definitions into `TypeIds`.
fn make_type_param_ids(type_params: &[TirTypeParam], tt: &mut TypeTable) -> Vec<TypeId> {
    type_params
        .iter()
        .map(|tp| tt.make_type_param(tp.name.clone(), tp.index))
        .collect()
}

type FieldInfo = (String, TypeId, u32);
type VariantCaseInfo = (String, u32, TypeId);

/// Collect non-generic struct info for trait synthesis.
fn collect_struct_fields(module: &TirModule) -> Vec<(String, Vec<FieldInfo>, Span)> {
    module
        .structs
        .iter()
        .filter(|s| s.type_params.is_empty() && s.monomorph_info.is_none())
        .map(|s| {
            let fields = s
                .fields
                .iter()
                .map(|f| (f.name.clone(), f.type_id, f.index))
                .collect();
            (s.name.clone(), fields, s.span)
        })
        .collect()
}

/// Collect generic struct info for trait synthesis.
fn collect_generic_struct_fields(
    module: &TirModule,
) -> Vec<(String, Vec<TirTypeParam>, Vec<FieldInfo>, Span)> {
    module
        .structs
        .iter()
        .filter(|s| !s.type_params.is_empty() && s.monomorph_info.is_none())
        .map(|s| {
            let fields = s
                .fields
                .iter()
                .map(|f| (f.name.clone(), f.type_id, f.index))
                .collect();
            (s.name.clone(), s.type_params.clone(), fields, s.span)
        })
        .collect()
}

/// Collect non-generic variant info for trait synthesis.
fn collect_variant_cases(module: &TirModule) -> Vec<(String, Vec<VariantCaseInfo>, Span)> {
    module
        .variants
        .iter()
        .filter(|v| v.type_params.is_empty())
        .map(|v| {
            let cases = v
                .cases
                .iter()
                .map(|c| (c.name.clone(), c.index, c.payload))
                .collect();
            (v.name.clone(), cases, v.span)
        })
        .collect()
}

/// Collect generic variant info for trait synthesis.
fn collect_generic_variant_cases(
    module: &TirModule,
) -> Vec<(String, Vec<TirTypeParam>, Vec<VariantCaseInfo>, Span)> {
    module
        .variants
        .iter()
        .filter(|v| !v.type_params.is_empty())
        .map(|v| {
            let cases = v
                .cases
                .iter()
                .map(|c| (c.name.clone(), c.index, c.payload))
                .collect();
            (v.name.clone(), v.type_params.clone(), cases, v.span)
        })
        .collect()
}

/// Collect non-generic struct info for Inspect/InspectAlt synthesis (excludes secret fields).
fn collect_struct_visible_fields(module: &TirModule) -> Vec<(String, Vec<FieldInfo>, bool, Span)> {
    module
        .structs
        .iter()
        .filter(|s| s.type_params.is_empty() && s.monomorph_info.is_none())
        .map(|s| {
            let fields = s
                .fields
                .iter()
                .filter(|f| !f.is_secret)
                .map(|f| (f.name.clone(), f.type_id, f.index))
                .collect();
            let has_secret = s.fields.iter().any(|f| f.is_secret);
            (s.name.clone(), fields, has_secret, s.span)
        })
        .collect()
}

/// Collect generic struct info for Inspect/InspectAlt synthesis (excludes secret fields).
fn collect_generic_struct_visible_fields(
    module: &TirModule,
) -> Vec<(String, Vec<TirTypeParam>, Vec<FieldInfo>, bool, Span)> {
    module
        .structs
        .iter()
        .filter(|s| !s.type_params.is_empty() && s.monomorph_info.is_none())
        .map(|s| {
            let fields = s
                .fields
                .iter()
                .filter(|f| !f.is_secret)
                .map(|f| (f.name.clone(), f.type_id, f.index))
                .collect();
            let has_secret = s.fields.iter().any(|f| f.is_secret);
            (
                s.name.clone(),
                s.type_params.clone(),
                fields,
                has_secret,
                s.span,
            )
        })
        .collect()
}

/// Generate auto-derived trait implementations (Eq, Ord) for enum types in a module.
fn generate_enum_trait_impls(module: &mut TirModule, ctx: &mut SynthesisCtx<'_, '_, '_>) {
    if module.enums.is_empty() {
        return;
    }

    let module_source = module.module_source.clone();
    let (eq_trait_name, ord_trait_name) = {
        let tt = module.type_table.borrow();
        let items = tt.compiler_items();
        (
            items
                .trait_name(crate::compiler_item::CompilerItem::Eq)
                .to_string(),
            items
                .trait_name(crate::compiler_item::CompilerItem::Ord)
                .to_string(),
        )
    };

    let enum_infos: Vec<_> = module
        .enums
        .iter()
        .map(|e| (e.name.clone(), e.span))
        .collect();

    let mut generated_functions = Vec::new();

    for (enum_name, span) in &enum_infos {
        let mut type_table = module.type_table.borrow_mut();
        let enum_type = type_table.make_enum(enum_name.clone(), module_source.clone());
        let ref_enum_type = type_table.make_ref(enum_type);

        if ctx.should_synthesize(enum_name, &eq_trait_name) {
            let func = generate_enum_eq_fn(
                &module_source,
                enum_name,
                enum_type,
                ref_enum_type,
                &eq_trait_name,
                *span,
            );
            generated_functions.push(Rc::new(RefCell::new(func)));
            ctx.record_impl(enum_name, &eq_trait_name);
        }

        if ctx.should_synthesize(enum_name, &ord_trait_name) {
            let ordering_type =
                type_table.make_compiler_enum(crate::compiler_item::CompilerItem::Ordering);
            let func = generate_enum_ord_fn(
                &module_source,
                enum_name,
                enum_type,
                ref_enum_type,
                ordering_type,
                &ord_trait_name,
                *span,
                ctx.names,
            );
            generated_functions.push(Rc::new(RefCell::new(func)));
            ctx.record_impl(enum_name, &ord_trait_name);
        }
    }

    module.functions.extend(generated_functions);
}

/// Generate auto-derived Eq and Ord trait implementations for struct types in a module.
///
/// For each struct whose fields all support Eq/Ord, generates:
/// - `StructName^Eq::eq(&self, &Self) -> bool` — field-wise equality
/// - `StructName^Ord::cmp(&self, &Self) -> Ordering` — lexicographic field comparison
///
/// Skips structs that already have user-provided implementations.
fn generate_struct_eq_ord_impls(module: &mut TirModule, ctx: &mut SynthesisCtx<'_, '_, '_>) {
    if module.structs.is_empty() {
        return;
    }

    let module_source = module.module_source.clone();
    let mut generated = Vec::new();

    let (eq_trait_name, ord_trait_name) = {
        let tt = module.type_table.borrow();
        let items = tt.compiler_items();
        (
            items
                .trait_name(crate::compiler_item::CompilerItem::Eq)
                .to_string(),
            items
                .trait_name(crate::compiler_item::CompilerItem::Ord)
                .to_string(),
        )
    };
    let mut tt = module.type_table.borrow_mut();

    let struct_infos = collect_struct_fields(module);
    for (name, fields, span) in &struct_infos {
        let struct_type = tt.make_struct(name.clone(), module_source.clone());
        let ref_struct_type = tt.make_ref(struct_type);

        if ctx.should_synthesize(name, &eq_trait_name) {
            let func = generate_struct_eq_fn(
                name,
                &[],
                fields,
                ref_struct_type,
                ctx.trait_env,
                &module_source,
                &eq_trait_name,
                &mut tt,
                *span,
            );
            generated.push(Rc::new(RefCell::new(func)));
            ctx.record_impl(name, &eq_trait_name);
        }

        if ctx.should_synthesize(name, &ord_trait_name) {
            let ordering_type = tt.make_compiler_enum(crate::compiler_item::CompilerItem::Ordering);
            let func = generate_struct_ord_fn(
                name,
                &[],
                fields,
                ref_struct_type,
                ordering_type,
                ctx.trait_env,
                &module_source,
                &ord_trait_name,
                &mut tt,
                *span,
                ctx.names,
            );
            generated.push(Rc::new(RefCell::new(func)));
            ctx.record_impl(name, &ord_trait_name);
        }
    }

    let generic_struct_infos = collect_generic_struct_fields(module);
    for (name, type_params, fields, span) in &generic_struct_infos {
        let type_param_ids = make_type_param_ids(type_params, &mut tt);
        let struct_type =
            tt.make_generic_instance(name.clone(), module_source.clone(), type_param_ids);
        let ref_struct_type = tt.make_ref(struct_type);

        if ctx.should_synthesize(name, &eq_trait_name) {
            let func = generate_struct_eq_fn(
                name,
                type_params,
                fields,
                ref_struct_type,
                ctx.trait_env,
                &module_source,
                &eq_trait_name,
                &mut tt,
                *span,
            );
            generated.push(Rc::new(RefCell::new(func)));
            ctx.record_impl(name, &eq_trait_name);
        }

        if ctx.should_synthesize(name, &ord_trait_name) {
            let ordering_type = tt.make_compiler_enum(crate::compiler_item::CompilerItem::Ordering);
            let func = generate_struct_ord_fn(
                name,
                type_params,
                fields,
                ref_struct_type,
                ordering_type,
                ctx.trait_env,
                &module_source,
                &ord_trait_name,
                &mut tt,
                *span,
                ctx.names,
            );
            generated.push(Rc::new(RefCell::new(func)));
            ctx.record_impl(name, &ord_trait_name);
        }
    }

    drop(tt);
    module.functions.extend(generated);
}

/// Generate auto-derived `Default` trait implementations for structs whose
/// fields all carry a declared default expression.
///
/// For a non-generic struct `S { f0: T0 = e0, f1: T1 = e1, ... }`, synthesize:
/// - `S^Default::default() -> S` — returns `S { f0: e0, f1: e1, ... }`.
///
/// Skips:
/// - structs where any field has no default expression,
/// - structs that already have a user-provided `impl Default for S`,
/// - generic structs (generic field defaults may depend on bounds; left for
///   a follow-up — monomorphized instances never hit this pass because
///   `monomorph_info.is_some()`).
///
/// Effect purity of the default expressions is already enforced by
/// `check_default_purity_semantic` before synthesis runs; if it had failed the
/// pipeline would have bailed, so every `default_expr` reaching here is pure.
fn generate_struct_default_impls(module: &mut TirModule, ctx: &mut SynthesisCtx<'_, '_, '_>) {
    if module.structs.is_empty() {
        return;
    }

    let module_source = module.module_source.clone();
    let mut generated = Vec::new();

    let default_trait_name = module
        .type_table
        .borrow()
        .compiler_trait_name(CompilerItem::Default)
        .to_string();
    let mut tt = module.type_table.borrow_mut();

    let infos: Vec<(String, Vec<(String, TypeId, u32, TirExpr)>, Span)> = module
        .structs
        .iter()
        .filter(|s| s.type_params.is_empty() && s.monomorph_info.is_none())
        .filter_map(|s| {
            let fields: Option<Vec<_>> = s
                .fields
                .iter()
                .map(|f| {
                    f.default_expr
                        .as_ref()
                        .map(|e| (f.name.clone(), f.type_id, f.index, (**e).clone()))
                })
                .collect();
            fields.map(|fields| (s.name.clone(), fields, s.span))
        })
        .collect();

    for (name, fields, span) in &infos {
        if !ctx.should_synthesize(name, &default_trait_name) {
            continue;
        }
        let struct_type = tt.make_struct(name.clone(), module_source.clone());
        let func = generate_struct_default_fn(
            &module_source,
            name,
            fields,
            struct_type,
            &default_trait_name,
            *span,
        );
        generated.push(Rc::new(RefCell::new(func)));
        ctx.record_impl(name, &default_trait_name);
    }

    drop(tt);
    module.functions.extend(generated);
}

/// Generate `StructName^Default::default() -> StructName` for a non-generic
/// struct whose fields all have default expressions.
fn generate_struct_default_fn(
    module_source: &ModuleSource,
    struct_name: &str,
    fields: &[(String, TypeId, u32, TirExpr)],
    struct_type: TypeId,
    default_trait_name: &str,
    span: Span,
) -> TirFunction {
    let method_info = trait_method_info(module_source, struct_name, default_trait_name, "default");
    let qualified_name = method_info.to_mangled_name();

    let struct_fields = fields
        .iter()
        .map(|(name, _type, index, value)| crate::tir::TirStructField {
            name: name.clone(),
            value: value.clone(),
            field_index: *index,
        })
        .collect();

    let literal = TirExpr::new(
        TirExprKind::StructLiteral {
            struct_type,
            struct_name: struct_name.to_string(),
            fields: struct_fields,
        },
        struct_type,
        span,
    );

    let body = TirBlock::new(
        vec![TirStmt::new(
            TirStmtKind::Return {
                value: Some(literal),
            },
            span,
        )],
        span,
    );

    make_synthetic_method(
        qualified_name,
        method_info,
        vec![],
        struct_type,
        body,
        vec![],
    )
}

/// Generate auto-derived Eq trait implementations for variant types in a module.
///
/// For each variant whose payload types all support Eq, generates:
/// - `VariantName^Eq::eq(&self, &Self) -> bool` — case-discriminated payload equality
///
/// Skips variants that already have user-provided implementations.
fn generate_variant_eq_impls(module: &mut TirModule, ctx: &mut SynthesisCtx<'_, '_, '_>) {
    if module.variants.is_empty() {
        return;
    }

    let module_source = module.module_source.clone();
    let mut generated = Vec::new();

    let eq_trait_name = module
        .type_table
        .borrow()
        .compiler_trait_name(crate::compiler_item::CompilerItem::Eq)
        .to_string();
    let mut tt = module.type_table.borrow_mut();

    let variant_infos = collect_variant_cases(module);
    for (name, cases, span) in &variant_infos {
        if !ctx.should_synthesize(name, &eq_trait_name) {
            continue;
        }
        let variant_type = tt.make_variant(name.clone(), module_source.clone());
        let ref_variant_type = tt.make_ref(variant_type);
        let func = generate_variant_eq_fn(
            name,
            &[],
            cases,
            variant_type,
            ref_variant_type,
            ctx.trait_env,
            &module_source,
            &mut tt,
            *span,
        );
        generated.push(Rc::new(RefCell::new(func)));
        ctx.record_impl(name, &eq_trait_name);
    }

    let generic_variant_infos = collect_generic_variant_cases(module);
    for (name, type_params, cases, span) in &generic_variant_infos {
        if !ctx.should_synthesize(name, &eq_trait_name) {
            continue;
        }
        let type_param_ids = make_type_param_ids(type_params, &mut tt);
        let variant_type =
            tt.make_generic_instance(name.clone(), module_source.clone(), type_param_ids);
        let ref_variant_type = tt.make_ref(variant_type);
        let func = generate_variant_eq_fn(
            name,
            type_params,
            cases,
            variant_type,
            ref_variant_type,
            ctx.trait_env,
            &module_source,
            &mut tt,
            *span,
        );
        generated.push(Rc::new(RefCell::new(func)));
        ctx.record_impl(name, &eq_trait_name);
    }

    drop(tt);
    module.functions.extend(generated);
}

/// Generate auto-derived `Inspect` trait implementations for all types in a module.
///
/// Generates `TypeName^Inspect::inspect(&self, &mut Formatter)` for:
/// - Enums: if-else chain writing type-qualified case names (e.g., `Color::Red`)
/// - Non-generic structs: writes field names and recursively inspects field values
/// - Generic structs: same with `impl_type_params` having Inspect bounds
/// - Non-generic variants: `VariantTest` dispatch with payload inspection
fn generate_inspect_impls(module: &mut TirModule, ctx: &mut SynthesisCtx<'_, '_, '_>) {
    let module_source = module.module_source.clone();
    let mut generated = Vec::new();
    let formatter_name = ctx.names.formatter.clone();
    let formatter_fq = ctx.names.formatter_fq.clone();
    let inspect_name = ctx.names.inspect.clone();
    let inspect_method = ctx.names.inspect_method.clone();
    let lower_hex_name = ctx.names.lower_hex.clone();
    let lower_hex_method = ctx.names.lower_hex_method.clone();

    let resource_infos: Vec<(String, Span)> = module
        .resources
        .iter()
        .filter(|r| !r.is_generic)
        .map(|r| (r.name.clone(), r.span))
        .collect();

    let mut tt = module.type_table.borrow_mut();
    let formatter_type = tt.make_struct(formatter_name.clone(), ModuleSource::format());
    let fmt_type = tt.make_mut_ref(formatter_type);
    let string_type = tt.make_compiler_struct(crate::compiler_item::CompilerItem::String);
    let ref_string_type = tt.make_ref(string_type);

    // Enum, variant, and flags types derive Inspect via their kind's blanket
    // in `core:prelude/traits` (WEP 2026-06-13), so nothing is emitted for
    // them here — nor for structs. What remains has no reflection: newtypes,
    // parameterized types, resources, and `Fn` dispatch stubs.

    // Newtypes (e.g., `type Meters = f64`)
    for nt in &module.newtypes {
        // Flags derive Inspect via the `ReflectFlags` blanket, not as newtypes.
        if module.flags.iter().any(|f| f.type_id == nt.type_id) {
            continue;
        }
        if ctx.has_methodful_impl_anywhere(&nt.name, &inspect_name) {
            continue;
        }
        let ResolvedType::Newtype { base_type, .. } = tt.get(nt.type_id) else {
            unreachable!("module.newtypes entry {} is not a Newtype type", nt.name);
        };
        let base_type = *base_type;
        let ref_type = tt.make_ref(nt.type_id);
        let span = synth_span();
        let as_suffix = write_str_stmt(
            // Strip the local-item storage disambiguator: a user must see
            // `as UserId`, never the internal `UserId@<local>` mangling.
            format!(" as {}", crate::name::strip_local_item_id(&nt.name)),
            local_expr(1, "f", fmt_type, span),
            string_type,
            ref_string_type,
            span,
            &ctx.names.formatter_fq,
        );
        generated.push(Rc::new(RefCell::new(generate_newtype_fmt_fn(
            &nt.name,
            nt.type_id,
            base_type,
            ref_type,
            fmt_type,
            ctx.trait_env,
            &module_source,
            &mut tt,
            span,
            &inspect_name,
            &inspect_method,
            Some(as_suffix),
        ))));
        ctx.record_impl(&nt.name, &inspect_name);
    }

    // Parameterized types (tuples, generic resources). `Fn` signatures are
    // handled separately below via `collect_canonical_fn_signatures` because
    // their dispatch stubs are keyed by `(arity, return_type)`, not `TypeId`.
    let span = synth_span();
    for (type_id, base_name, type_arg_names) in collect_parameterized_types(&tt) {
        let mangled = tt.fq_type_name(type_id).to_mangled();
        if ctx.has_methodful_impl_anywhere(&mangled, &inspect_name) {
            continue;
        }
        let ref_type = tt.make_ref(type_id);
        let resolved = tt.get(type_id).clone();
        match resolved {
            ResolvedType::GenericInstance {
                ref name,
                ref module_source,
                ..
            } if TypeTable::is_tuple_type(name) => {
                // Tuple Inspect is provided by variadic impl in core:prelude/tuple.wado
            }
            _ => {
                // A name carries its subject's declaring module, so one name is
                // one function: the stub belongs to the module declaring the
                // shape's base and is emitted once, there. Emitting a copy into
                // every using module — which the bare-name scheme needed, since
                // each module's copy then had a distinct identity — now mints
                // several functions under one name, and a call from a third
                // module matches none of them.
                // A shape with no declaration (a tuple, a reference, a `Fn`)
                // has no module to be named by, so it keeps a copy per using
                // module — the same reason the `Fn` arm below does.
                let shape_module = match shape_declaring_module(&resolved) {
                    Some(m) if m != module_source => continue,
                    Some(m) => m,
                    None => module_source.clone(),
                };
                let type_name = tt.type_name(type_id);
                generated.push(Rc::new(RefCell::new(generate_opaque_inspect_fn(
                    &base_name,
                    &type_arg_names,
                    &type_name,
                    type_id,
                    ref_type,
                    fmt_type,
                    string_type,
                    ref_string_type,
                    ctx.trait_env,
                    &shape_module,
                    &mut tt,
                    span,
                    &inspect_name,
                    &inspect_method,
                    &formatter_fq,
                    &lower_hex_name,
                    &lower_hex_method,
                ))));
            }
        }
    }

    for (name, rspan) in &resource_infos {
        if ctx.has_methodful_impl_anywhere(name, &inspect_name) {
            continue;
        }
        let resource_type = tt.make_resource(name.clone(), module_source.clone());
        let ref_type = tt.make_ref(resource_type);
        generated.push(Rc::new(RefCell::new(generate_opaque_inspect_fn(
            name,
            &[],
            name,
            resource_type,
            ref_type,
            fmt_type,
            string_type,
            ref_string_type,
            ctx.trait_env,
            &module_source,
            &mut tt,
            *rspan,
            &inspect_name,
            &inspect_method,
            &formatter_fq,
            &lower_hex_name,
            &lower_hex_method,
        ))));
        ctx.record_impl(name, &inspect_name);
    }

    // `Fn` dispatch stubs — one per canonical `(arity, return_type)`.
    for sig in collect_canonical_fn_signatures(&tt) {
        let mangled = sig.receiver().to_mangled();
        if ctx.has_methodful_impl_anywhere(&mangled, &inspect_name) {
            continue;
        }
        let ref_type = tt.make_ref(sig.repr_type_id);
        generated.push(Rc::new(RefCell::new(generate_fn_inspect_fn(
            &module_source,
            &sig.type_arg_names,
            sig.arity,
            sig.return_type,
            ref_type,
            fmt_type,
            span,
            &inspect_name,
            &inspect_method,
        ))));
        // Per-module: do not `ctx.record_impl`.
    }

    drop(tt);
    module.functions.extend(generated);
}

/// Auto-derive `EnumName^Display::fmt` writing the bare case name (`Red`),
/// distinct from `Inspect`'s type-qualified `Color::Red`. Skips enums with a
/// user-written `Display` impl.
fn generate_enum_display_impls(module: &mut TirModule, ctx: &mut SynthesisCtx<'_, '_, '_>) {
    if module.enums.is_empty() {
        return;
    }

    let module_source = module.module_source.clone();
    let display_name = ctx.names.display.clone();
    let display_method = ctx.names.display_method.clone();
    let formatter_name = ctx.names.formatter.clone();
    let formatter_fq = ctx.names.formatter_fq.clone();

    let enum_infos: Vec<_> = module
        .enums
        .iter()
        .map(|e| {
            let cases: Vec<_> = e.cases.iter().map(|c| (c.name.clone(), c.index)).collect();
            (e.name.clone(), cases, e.span)
        })
        .collect();

    let mut tt = module.type_table.borrow_mut();
    let formatter_type = tt.make_struct(formatter_name.clone(), ModuleSource::format());
    let fmt_type = tt.make_mut_ref(formatter_type);
    let string_type = tt.make_compiler_struct(crate::compiler_item::CompilerItem::String);
    let ref_string_type = tt.make_ref(string_type);

    let mut generated = Vec::new();
    for (name, cases, espan) in &enum_infos {
        if ctx.has_methodful_impl_anywhere(name, &display_name) {
            continue;
        }
        let enum_type = tt.make_enum(name.clone(), module_source.clone());
        let ref_type = tt.make_ref(enum_type);
        generated.push(Rc::new(RefCell::new(generate_enum_display_fn(
            &module_source,
            name,
            cases,
            enum_type,
            ref_type,
            fmt_type,
            string_type,
            ref_string_type,
            *espan,
            &display_name,
            &display_method,
            &formatter_fq,
        ))));
        ctx.record_impl(name, &display_name);
    }

    drop(tt);
    module.functions.extend(generated);
}

/// Generate `EnumName^Display::fmt(&self, &mut Formatter)` writing the bare case
/// name. Mirrors [`generate_enum_inspect_fn`] but omits the `EnumName::` prefix.
#[allow(clippy::too_many_arguments)]
fn generate_enum_display_fn(
    module_source: &ModuleSource,
    enum_name: &str,
    cases: &[(String, u32)],
    enum_type: TypeId,
    ref_enum_type: TypeId,
    fmt_type: TypeId,
    string_type: TypeId,
    ref_string_type: TypeId,
    span: Span,
    display_trait: &str,
    display_method: &str,
    formatter_fq: &FqTypeName,
) -> TirFunction {
    let method_info = trait_method_info(module_source, enum_name, display_trait, display_method);
    let qualified_name = method_info.to_mangled_name();

    let deref_self = || deref_local(0, "self", ref_enum_type, enum_type, span);
    let fmt = || local_expr(1, "f", fmt_type, span);

    let mut chain: Option<TirExpr> = None;
    for (case_name, case_index) in cases.iter().rev() {
        let then_block = TirBlock::new(
            vec![write_str_stmt(
                case_name.clone(),
                fmt(),
                string_type,
                ref_string_type,
                span,
                &formatter_fq,
            )],
            span,
        );
        let cond = TirExpr::new(
            TirExprKind::Binary {
                left: Box::new(deref_self()),
                op: TirBinaryOp::Eq,
                right: Box::new(TirExpr::new(
                    TirExprKind::IntLiteral {
                        value: u64::from(*case_index),
                        repr: case_index.to_string(),
                    },
                    enum_type,
                    span,
                )),
            },
            TypeTable::BOOL,
            span,
        );
        let if_expr = TirExpr::new(
            TirExprKind::If {
                condition: Box::new(cond),
                then_branch: then_block,
                else_branch: chain
                    .map(|e| TirBlock::new(vec![TirStmt::new(TirStmtKind::Expr(e), span)], span)),
            },
            TypeTable::UNIT,
            span,
        );
        chain = Some(if_expr);
    }

    let stmts = chain.map_or_else(Vec::new, |e| vec![TirStmt::new(TirStmtKind::Expr(e), span)]);
    let body = TirBlock::new(stmts, span);

    make_synthetic_method(
        qualified_name,
        method_info,
        inspect_params(ref_enum_type, fmt_type, span),
        TypeTable::UNIT,
        body,
        inspect_locals(ref_enum_type, fmt_type),
    )
}
/// Generate `NewtypeName^<Trait>::<method>(&self, &mut Formatter)` for a
/// newtype: delegate to the base type's same method via `(self as Base).<method>(f)`,
/// then append `suffix`.
///
/// `Inspect` passes `Some(write_str(" as NewtypeName"))` so debug output reads
/// `100.5 as Meters`; `Display` passes `None` so it renders transparently like
/// the base value.
fn generate_newtype_fmt_fn(
    newtype_name: &str,
    newtype_type: TypeId,
    base_type: TypeId,
    ref_newtype_type: TypeId,
    fmt_type: TypeId,
    trait_env: &TraitEnv,
    module_source: &ModuleSource,
    tt: &mut TypeTable,
    span: Span,
    fmt_trait: &str,
    fmt_method: &str,
    suffix: Option<TirStmt>,
) -> TirFunction {
    let method_info = trait_method_info(module_source, newtype_name, fmt_trait, fmt_method);
    let qualified_name = method_info.to_mangled_name();

    let deref_self = deref_local(0, "self", ref_newtype_type, newtype_type, span);
    let cast_to_base = TirExpr::new(
        TirExprKind::Cast {
            expr: Box::new(deref_self),
            target_type: base_type,
        },
        base_type,
        span,
    );

    let mut stmts = vec![inspect_call(
        cast_to_base,
        base_type,
        local_expr(1, "f", fmt_type, span),
        trait_env,
        module_source,
        tt,
        span,
        fmt_trait,
        fmt_method,
    )];
    stmts.extend(suffix);

    make_synthetic_method(
        qualified_name,
        method_info,
        inspect_params(ref_newtype_type, fmt_type, span),
        TypeTable::UNIT,
        TirBlock::new(stmts, span),
        inspect_locals(ref_newtype_type, fmt_type),
    )
}

/// Generate `Fn<N, Ret>^Inspect::inspect(&self, &mut Formatter)` as
/// an auto-derived dispatch stub.
///
/// The TIR body is `None` — the function entry exists only so call
/// sites referring to it from templates / user code resolve. A bodyless
/// TIR function naturally bypasses the inliner and other body walkers;
/// WIR build recognises [`FunctionKind::FnCanonicalDispatch`] and
/// supplies the real body: a `call_ref` through the matching
/// `CanonicalClosure_K`'s `inspect` vtable slot. See WEP: Inspect
/// (Debug Output) > Closure Inspect via Runtime Dispatch.
fn generate_fn_inspect_fn(
    module_source: &ModuleSource,
    type_arg_names: &[FqTypeName],
    arity: usize,
    return_type: TypeId,
    ref_fn_type: TypeId,
    fmt_type: TypeId,
    span: Span,
    inspect_trait: &str,
    inspect_method: &str,
) -> TirFunction {
    generate_fn_canonical_dispatch_stub(
        &module_source,
        FnDispatchTrait::Inspect,
        inspect_trait,
        inspect_method,
        type_arg_names,
        arity,
        return_type,
        ref_fn_type,
        fmt_type,
        span,
    )
}

/// Twin of [`generate_fn_inspect_fn`] for `Fn<N, Ret>^InspectAlt`.
fn generate_fn_inspect_alt_fn(
    module_source: &ModuleSource,
    type_arg_names: &[FqTypeName],
    arity: usize,
    return_type: TypeId,
    ref_fn_type: TypeId,
    fmt_type: TypeId,
    span: Span,
    inspect_alt_trait: &str,
    inspect_alt_method: &str,
) -> TirFunction {
    generate_fn_canonical_dispatch_stub(
        &module_source,
        FnDispatchTrait::InspectAlt,
        inspect_alt_trait,
        inspect_alt_method,
        type_arg_names,
        arity,
        return_type,
        ref_fn_type,
        fmt_type,
        span,
    )
}

/// Shared body for [`generate_fn_inspect_fn`] /
/// [`generate_fn_inspect_alt_fn`]. The two only differ in the trait
/// label and the `FnDispatchTrait` carried in `FunctionKind`.
#[allow(clippy::too_many_arguments)]
fn generate_fn_canonical_dispatch_stub(
    module_source: &ModuleSource,
    trait_kind: FnDispatchTrait,
    trait_name: &str,
    method_name: &str,
    type_arg_names: &[FqTypeName],
    arity: usize,
    return_type: TypeId,
    ref_fn_type: TypeId,
    fmt_type: TypeId,
    span: Span,
) -> TirFunction {
    let method_info = trait_method_info(
        module_source,
        crate::name::CLOSURE_FN_TRAIT,
        trait_name,
        method_name,
    )
    .with_struct_type_args(type_arg_names);
    let qualified_name = method_info.to_mangled_name();

    let mut func = make_synthetic_method(
        qualified_name,
        method_info,
        inspect_params(ref_fn_type, fmt_type, span),
        TypeTable::UNIT,
        TirBlock::new(vec![], span),
        inspect_locals(ref_fn_type, fmt_type),
    );
    // No TIR body: the real instructions are supplied at WIR build
    // time via the `FnCanonicalDispatch` arm in `translate_function_bodies`.
    // Bodyless functions are naturally skipped by the inliner and other
    // TIR-body walkers, so no `InlineHint::Never` is needed.
    func.body = None;
    func.kind = FunctionKind::FnCanonicalDispatch {
        trait_kind,
        arity,
        return_type,
    };
    func
}

/// Generate Inspect for opaque/resource types (Future, Stream, etc.).
///
/// Generate `Inspect` for an opaque resource handle, rendered as `Name#0x<hex>`.
#[allow(clippy::too_many_arguments)]
fn generate_opaque_inspect_fn(
    base_name: &str,
    type_arg_names: &[FqTypeName],
    type_name: &str,
    resource_type: TypeId,
    ref_type: TypeId,
    fmt_type: TypeId,
    string_type: TypeId,
    ref_string_type: TypeId,
    trait_env: &TraitEnv,
    module_source: &ModuleSource,
    tt: &mut TypeTable,
    span: Span,
    inspect_trait: &str,
    inspect_method: &str,
    formatter_fq: &FqTypeName,
    lower_hex_trait: &str,
    lower_hex_method: &str,
) -> TirFunction {
    let method_info = trait_method_info(module_source, base_name, inspect_trait, inspect_method)
        .with_struct_type_args(type_arg_names);
    let qualified_name = method_info.to_mangled_name();

    let fmt = || local_expr(1, "f", fmt_type, span);
    let deref_self = deref_local(0, "self", ref_type, resource_type, span);
    let handle = TirExpr::new(
        TirExprKind::Cast {
            expr: Box::new(deref_self),
            target_type: TypeTable::I32,
        },
        TypeTable::I32,
        span,
    );
    let hex_stmt = inspect_call(
        handle,
        TypeTable::I32,
        fmt(),
        trait_env,
        module_source,
        tt,
        span,
        lower_hex_trait,
        lower_hex_method,
    );
    let body = TirBlock::new(
        vec![
            write_str_stmt(
                format!("{type_name}#0x"),
                fmt(),
                string_type,
                ref_string_type,
                span,
                &formatter_fq,
            ),
            hex_stmt,
        ],
        span,
    );

    make_synthetic_method(
        qualified_name,
        method_info,
        inspect_params(ref_type, fmt_type, span),
        TypeTable::UNIT,
        body,
        inspect_locals(ref_type, fmt_type),
    )
}

/// Generate auto-derived `InspectAlt` trait implementations for all types in a module.
///
/// For composite types (structs, variants, arrays, tuples), generates pretty-printed
/// multi-line output using Formatter's `begin_block`/`end_block`/`write_field_sep` helpers.
/// For simple types (enums, flags, newtypes, primitives, functions), delegates to Inspect.
fn generate_inspect_alt_impls(module: &mut TirModule, ctx: &mut SynthesisCtx<'_, '_, '_>) {
    let module_source = module.module_source.clone();
    let formatter_name = ctx.names.formatter.clone();
    let formatter_fq = ctx.names.formatter_fq.clone();
    let inspect_name = ctx.names.inspect.clone();
    let inspect_method = ctx.names.inspect_method.clone();
    let inspect_alt_name = ctx.names.inspect_alt.clone();
    let inspect_alt_method = ctx.names.inspect_alt_method.clone();
    let all_fn_names: IndexSet<String> = module
        .functions
        .iter()
        .filter_map(|f| f.try_borrow().ok().map(|func| func.name.clone()))
        .collect();
    let mut generated = Vec::new();

    let mut tt = module.type_table.borrow_mut();
    let formatter_type = tt.make_struct(formatter_name.clone(), ModuleSource::format());
    let fmt_type = tt.make_mut_ref(formatter_type);
    let string_type = tt.make_compiler_struct(crate::compiler_item::CompilerItem::String);
    let ref_string_type = tt.make_ref(string_type);
    let span = synth_span();

    // `Inspect` reaches a type by three routes, and a delegate is warranted
    // whichever one applies: a trait impl (via TraitEnv / the synthesis
    // layer), a free function with the same mangled name (legacy stdlib code
    // predating trait synthesis), or one of the `Reflect*` blankets — the
    // route every plain declaration takes, which leaves no per-type impl to
    // find.
    let has_inspect = |type_name: &str,
                       type_id: TypeId,
                       ctx: &SynthesisCtx<'_, '_, '_>,
                       tt: &mut TypeTable|
     -> bool {
        if ctx.has_impl(type_name, &inspect_name) {
            return true;
        }
        let mangled = MethodName::format_local(
            &FqTypeName::of_head(&module_source, type_name),
            Some(&inspect_name),
            &inspect_method,
        );
        if all_fn_names.contains(&mangled) {
            return true;
        }
        crate::synthesis::template::blanket_dispatch_for(
            ctx.trait_env,
            type_id,
            &inspect_name,
            &inspect_method,
            tt,
        )
        .is_some()
    };

    // Enums — delegate to Inspect (no multiline needed for enum names)
    for name in module
        .enums
        .iter()
        .map(|e| e.name.clone())
        .collect::<Vec<_>>()
    {
        if ctx.has_methodful_impl_anywhere(&name, &inspect_alt_name) {
            continue;
        }
        let enum_type = tt.make_enum(name.clone(), module_source.clone());
        if !has_inspect(&name, enum_type, ctx, &mut tt) {
            continue;
        }
        let ref_type = tt.make_ref(enum_type);
        generated.push(Rc::new(RefCell::new(generate_display_fallback(
            trait_method_info(
                &module_source,
                &name,
                &inspect_alt_name,
                &inspect_alt_method,
            ),
            trait_method_info(&module_source, &name, &inspect_name, &inspect_method),
            ref_type,
            enum_type,
            fmt_type,
            ctx.trait_env,
            &module_source,
            vec![],
            &mut tt,
            span,
        ))));
        ctx.record_impl(&name, &inspect_alt_name);
    }

    // Non-generic structs — pretty-print with begin_block/end_block
    let struct_infos = collect_struct_visible_fields(module);

    for (name, fields, has_secret, sspan) in &struct_infos {
        if name == tt.compiler_struct_name(crate::compiler_item::CompilerItem::String)
            || name == &formatter_name
        {
            continue;
        }
        if ctx.has_methodful_impl_anywhere(name, &inspect_alt_name) {
            continue;
        }
        let struct_type = tt.make_struct(name.clone(), module_source.clone());
        let ref_type = tt.make_ref(struct_type);
        generated.push(Rc::new(RefCell::new(generate_struct_inspect_alt_fn(
            name,
            &[],
            fields,
            *has_secret,
            ref_type,
            fmt_type,
            string_type,
            ref_string_type,
            ctx.trait_env,
            &module_source,
            &mut tt,
            *sspan,
            &inspect_alt_name,
            &inspect_alt_method,
            &formatter_fq,
        ))));
        ctx.record_impl(name, &inspect_alt_name);
    }

    let generic_struct_infos = collect_generic_struct_visible_fields(module);
    for (name, type_params, fields, has_secret, sspan) in &generic_struct_infos {
        if ctx.has_methodful_impl_anywhere(name, &inspect_alt_name) {
            continue;
        }
        let type_param_ids = make_type_param_ids(type_params, &mut tt);
        let struct_type =
            tt.make_generic_instance(name.clone(), module_source.clone(), type_param_ids);
        let ref_type = tt.make_ref(struct_type);
        generated.push(Rc::new(RefCell::new(generate_struct_inspect_alt_fn(
            name,
            type_params,
            fields,
            *has_secret,
            ref_type,
            fmt_type,
            string_type,
            ref_string_type,
            ctx.trait_env,
            &module_source,
            &mut tt,
            *sspan,
            &inspect_alt_name,
            &inspect_alt_method,
            &formatter_fq,
        ))));
        ctx.record_impl(name, &inspect_alt_name);
    }

    let variant_infos = collect_variant_cases(module);
    for (name, cases, vspan) in &variant_infos {
        if ctx.has_methodful_impl_anywhere(name, &inspect_alt_name) {
            continue;
        }
        let variant_type = tt.make_variant(name.clone(), module_source.clone());
        let ref_type = tt.make_ref(variant_type);
        generated.push(Rc::new(RefCell::new(generate_variant_inspect_alt_fn(
            name,
            &[],
            cases,
            variant_type,
            ref_type,
            fmt_type,
            string_type,
            ref_string_type,
            ctx.trait_env,
            &module_source,
            &mut tt,
            *vspan,
            &inspect_alt_name,
            &inspect_alt_method,
            &formatter_fq,
        ))));
        ctx.record_impl(name, &inspect_alt_name);
    }

    let generic_variant_infos = collect_generic_variant_cases(module);
    for (name, type_params, cases, vspan) in &generic_variant_infos {
        if ctx.has_methodful_impl_anywhere(name, &inspect_alt_name) {
            continue;
        }
        let type_param_ids = make_type_param_ids(type_params, &mut tt);
        let variant_type =
            tt.make_generic_instance(name.clone(), module_source.clone(), type_param_ids);
        let ref_type = tt.make_ref(variant_type);
        generated.push(Rc::new(RefCell::new(generate_variant_inspect_alt_fn(
            name,
            type_params,
            cases,
            variant_type,
            ref_type,
            fmt_type,
            string_type,
            ref_string_type,
            ctx.trait_env,
            &module_source,
            &mut tt,
            *vspan,
            &inspect_alt_name,
            &inspect_alt_method,
            &formatter_fq,
        ))));
        ctx.record_impl(name, &inspect_alt_name);
    }

    // Flags — delegate to Inspect (bit flags don't need pretty print)
    let flags_infos: Vec<_> = module
        .flags
        .iter()
        .map(|f| (f.name.clone(), f.type_id))
        .collect();

    for (name, flags_type_id) in &flags_infos {
        if ctx.has_methodful_impl_anywhere(name, &inspect_alt_name)
            || !has_inspect(name, *flags_type_id, ctx, &mut tt)
        {
            continue;
        }
        let ref_type = tt.make_ref(*flags_type_id);
        generated.push(Rc::new(RefCell::new(generate_display_fallback(
            trait_method_info(&module_source, name, &inspect_alt_name, &inspect_alt_method),
            trait_method_info(&module_source, name, &inspect_name, &inspect_method),
            ref_type,
            *flags_type_id,
            fmt_type,
            ctx.trait_env,
            &module_source,
            vec![],
            &mut tt,
            span,
        ))));
        ctx.record_impl(name, &inspect_alt_name);
    }

    for nt in &module.newtypes {
        if module.flags.iter().any(|f| f.type_id == nt.type_id) {
            continue;
        }
        if ctx.has_methodful_impl_anywhere(&nt.name, &inspect_alt_name)
            || !has_inspect(&nt.name, nt.type_id, ctx, &mut tt)
        {
            continue;
        }
        let ref_type = tt.make_ref(nt.type_id);
        generated.push(Rc::new(RefCell::new(generate_display_fallback(
            trait_method_info(
                &module_source,
                &nt.name,
                &inspect_alt_name,
                &inspect_alt_method,
            ),
            trait_method_info(&module_source, &nt.name, &inspect_name, &inspect_method),
            ref_type,
            nt.type_id,
            fmt_type,
            ctx.trait_env,
            &module_source,
            vec![],
            &mut tt,
            span,
        ))));
        ctx.record_impl(&nt.name, &inspect_alt_name);
    }

    // Tuples are skipped — their `InspectAlt` is provided by the variadic impl
    // in `core:prelude/tuple.wado`. Opaque resource types delegate to their
    // `Inspect` counterpart. `Fn` signatures are handled separately below.
    for (type_id, base_name, type_arg_names) in collect_parameterized_types(&tt) {
        let mangled = tt.fq_type_name(type_id).to_mangled();
        if ctx.has_methodful_impl_anywhere(&mangled, &inspect_alt_name)
            || !has_inspect(&mangled, type_id, ctx, &mut tt)
        {
            continue;
        }
        let resolved = tt.get(type_id).clone();
        if matches!(resolved, ResolvedType::GenericInstance { ref name, ref module_source, .. } if TypeTable::is_tuple_type(name))
        {
            // Tuple InspectAlt is provided by variadic impl in core:prelude/tuple.wado
            continue;
        }
        // One name is one function, so the delegating impl belongs to the
        // module declaring the shape's base and is emitted once, there —
        // matching where its `Inspect` counterpart lands.
        let shape_module = match shape_declaring_module(&resolved) {
            Some(m) if m != module_source => continue,
            Some(m) => m,
            None => module_source.clone(),
        };
        let ref_type = tt.make_ref(type_id);
        // Opaque resource types (Future, Stream, etc.): delegate to
        // Inspect via the stock `display_fallback`.
        generated.push(Rc::new(RefCell::new(generate_display_fallback(
            trait_method_info(
                &shape_module,
                &base_name,
                &inspect_alt_name,
                &inspect_alt_method,
            )
            .with_struct_type_args(&type_arg_names),
            trait_method_info(&shape_module, &base_name, &inspect_name, &inspect_method)
                .with_struct_type_args(&type_arg_names),
            ref_type,
            type_id,
            fmt_type,
            ctx.trait_env,
            &shape_module,
            vec![],
            &mut tt,
            span,
        ))));
        // Per-module: do not `ctx.record_impl`.
    }

    // `Fn` dispatch stubs — one per canonical `(arity, return_type)`.
    // Crucially, do NOT use the `display_fallback` Inspect-delegate:
    // WIR build supplies the real body — `call_ref (self.inspect_alt)`
    // for InspectAlt, `call_ref (self.inspect)` for Inspect — and a
    // delegate would let the optimizer collapse InspectAlt to Inspect
    // before WIR build runs, defeating the per-literal source dispatch.
    for sig in collect_canonical_fn_signatures(&tt) {
        let mangled = sig.receiver().to_mangled();
        if ctx.has_methodful_impl_anywhere(&mangled, &inspect_alt_name)
            || !has_inspect(&mangled, sig.repr_type_id, ctx, &mut tt)
        {
            continue;
        }
        let ref_type = tt.make_ref(sig.repr_type_id);
        generated.push(Rc::new(RefCell::new(generate_fn_inspect_alt_fn(
            &module_source,
            &sig.type_arg_names,
            sig.arity,
            sig.return_type,
            ref_type,
            fmt_type,
            span,
            &inspect_alt_name,
            &inspect_alt_method,
        ))));
        // Per-module: do not `ctx.record_impl`.
    }

    drop(tt);
    module.functions.extend(generated);
}

/// Generate `StructName^InspectAlt::inspect_alt` for non-generic structs (pretty-print).
///
/// Body uses Formatter helpers for clean synthesis:
/// ```text
/// f.begin_block("StructName {\n");
/// f.write_indent(); f.write_str("field1: "); self.field1.inspect_alt(f); f.write_str(",\n");
/// f.write_indent(); f.write_str("field2: "); self.field2.inspect_alt(f); f.write_str(",\n");
/// f.end_block("}");
/// ```
/// Pass an empty `impl_type_params` slice for non-generic structs.
fn generate_struct_inspect_alt_fn(
    struct_name: &str,
    impl_type_params: &[TirTypeParam],
    fields: &[(String, TypeId, u32)],
    has_secret: bool,
    ref_struct_type: TypeId,
    fmt_type: TypeId,
    string_type: TypeId,
    ref_string_type: TypeId,
    trait_env: &TraitEnv,
    module_source: &ModuleSource,
    tt: &mut TypeTable,
    span: Span,
    inspect_alt_trait: &str,
    inspect_alt_method: &str,
    formatter_fq: &FqTypeName,
) -> TirFunction {
    let method_info = trait_method_info(
        module_source,
        struct_name,
        inspect_alt_trait,
        inspect_alt_method,
    );
    let qualified_name = method_info.to_mangled_name();

    let stmts = build_struct_inspect_alt_body(
        struct_name,
        fields,
        has_secret,
        ref_struct_type,
        fmt_type,
        string_type,
        ref_string_type,
        trait_env,
        module_source,
        tt,
        span,
        formatter_fq,
        inspect_alt_trait,
        inspect_alt_method,
    );
    let body = TirBlock::new(stmts, span);

    make_trait_method(
        qualified_name,
        method_info,
        impl_type_params.to_vec(),
        inspect_params(ref_struct_type, fmt_type, span),
        TypeTable::UNIT,
        body,
        inspect_locals(ref_struct_type, fmt_type),
        span,
    )
}

/// Build the body statements for struct `InspectAlt`: pretty-printed multi-line output
/// using `Formatter::open_brace`/`close_brace`/`write_newline_indent`.
fn build_struct_inspect_alt_body(
    struct_name: &str,
    fields: &[(String, TypeId, u32)],
    has_secret: bool,
    ref_struct_type: TypeId,
    fmt_type: TypeId,
    string_type: TypeId,
    ref_string_type: TypeId,
    trait_env: &TraitEnv,
    module_source: &ModuleSource,
    tt: &mut TypeTable,
    span: Span,
    formatter_fq: &FqTypeName,
    inspect_alt_trait: &str,
    inspect_alt_method: &str,
) -> Vec<TirStmt> {
    let fmt = || local_expr(1, "f", fmt_type, span);
    let write = |s: &str| {
        write_str_stmt(
            s.to_string(),
            fmt(),
            string_type,
            ref_string_type,
            span,
            &formatter_fq,
        )
    };
    let newline_indent = || {
        formatter_call(
            "write_newline_indent",
            fmt(),
            None::<(&str, TypeId)>,
            span,
            &formatter_fq,
        )
    };

    let mut stmts = Vec::new();

    if fields.is_empty() {
        let suffix = if has_secret { " { .. }" } else { " {}" };
        stmts.push(write_str_stmt(
            format!("{struct_name}{suffix}"),
            fmt(),
            string_type,
            ref_string_type,
            span,
            &formatter_fq,
        ));
        return stmts;
    }

    stmts.push(formatter_call(
        "open_brace",
        fmt(),
        Some((format!("{struct_name} {{"), string_type)),
        span,
        &formatter_fq,
    ));
    for (field_name, field_type, field_index) in fields {
        stmts.push(newline_indent());
        stmts.push(write_str_stmt(
            format!("{field_name}: "),
            fmt(),
            string_type,
            ref_string_type,
            span,
            &formatter_fq,
        ));
        let field_access = field_access_local(
            0,
            "self",
            ref_struct_type,
            *field_index,
            field_name,
            *field_type,
            span,
        );
        stmts.push(inspect_alt_call(
            field_access,
            *field_type,
            fmt(),
            trait_env,
            module_source,
            tt,
            span,
            inspect_alt_trait,
            inspect_alt_method,
        ));
        stmts.push(write(","));
    }
    if has_secret {
        stmts.push(newline_indent());
        stmts.push(write(".."));
    }
    stmts.push(formatter_call(
        "close_brace",
        fmt(),
        Some(("}", string_type)),
        span,
        &formatter_fq,
    ));
    stmts
}

/// Generate `VariantName^InspectAlt::inspect_alt`.
///
/// Pass an empty `impl_type_params` slice for non-generic variants.
fn generate_variant_inspect_alt_fn(
    variant_name: &str,
    impl_type_params: &[TirTypeParam],
    cases: &[(String, u32, TypeId)],
    variant_type: TypeId,
    ref_variant_type: TypeId,
    fmt_type: TypeId,
    string_type: TypeId,
    ref_string_type: TypeId,
    trait_env: &TraitEnv,
    module_source: &ModuleSource,
    tt: &mut TypeTable,
    span: Span,
    inspect_alt_trait: &str,
    inspect_alt_method: &str,
    formatter_fq: &FqTypeName,
) -> TirFunction {
    let method_info = trait_method_info(
        module_source,
        variant_name,
        inspect_alt_trait,
        inspect_alt_method,
    );
    let qualified_name = method_info.to_mangled_name();

    // Same payload-binding allocation strategy as
    // `generate_variant_inspect_fn`: each non-unit case reserves a
    // local slot after the two parameter locals so the synthesised
    // `TirPattern::Variant { bindings }` arm can refer to it.
    let mut locals = inspect_locals(ref_variant_type, fmt_type);
    let mut payload_bindings: Vec<Option<u32>> = Vec::with_capacity(cases.len());
    for (case_name, _, payload_type) in cases {
        if *payload_type == TypeTable::UNIT {
            payload_bindings.push(None);
        } else {
            let idx = locals.len() as u32;
            locals.push(param_local(
                &format!("__inspect_alt_{case_name}_{idx}"),
                *payload_type,
                false,
            ));
            payload_bindings.push(Some(idx));
        }
    }

    let stmts = build_variant_inspect_alt_body(
        variant_name,
        cases,
        &payload_bindings,
        variant_type,
        ref_variant_type,
        fmt_type,
        string_type,
        ref_string_type,
        trait_env,
        module_source,
        tt,
        span,
        formatter_fq,
        inspect_alt_trait,
        inspect_alt_method,
    );
    let body = TirBlock::new(stmts, span);

    make_trait_method(
        qualified_name,
        method_info,
        impl_type_params.to_vec(),
        inspect_params(ref_variant_type, fmt_type, span),
        TypeTable::UNIT,
        body,
        locals,
        span,
    )
}

/// Build the body for variant `InspectAlt` (shared between generic and
/// non-generic) as a single TIR `Match` over `*self`.
fn build_variant_inspect_alt_body(
    variant_name: &str,
    cases: &[(String, u32, TypeId)],
    payload_bindings: &[Option<u32>],
    variant_type: TypeId,
    ref_variant_type: TypeId,
    fmt_type: TypeId,
    string_type: TypeId,
    ref_string_type: TypeId,
    trait_env: &TraitEnv,
    module_source: &ModuleSource,
    tt: &mut TypeTable,
    span: Span,
    formatter_fq: &FqTypeName,
    inspect_alt_trait: &str,
    inspect_alt_method: &str,
) -> Vec<TirStmt> {
    if cases.is_empty() {
        return Vec::new();
    }

    let deref_self = deref_expr(
        local_expr(0, "self", ref_variant_type, span),
        variant_type,
        span,
    );
    let fmt_local = || local_expr(1, "f", fmt_type, span);

    let arms: Vec<TirMatchArm> = cases
        .iter()
        .zip(payload_bindings.iter())
        .map(|((case_name, _, payload_type), binding_idx)| {
            let is_unit = *payload_type == TypeTable::UNIT;
            let mut body_stmts = Vec::new();
            let bindings: Vec<TirPattern> = if is_unit {
                body_stmts.push(write_str_stmt(
                    format!("{variant_name}::{case_name}"),
                    fmt_local(),
                    string_type,
                    ref_string_type,
                    span,
                    &formatter_fq,
                ));
                Vec::new()
            } else {
                body_stmts.push(formatter_call(
                    "open_brace",
                    fmt_local(),
                    Some((format!("{variant_name}::{case_name}("), string_type)),
                    span,
                    &formatter_fq,
                ));
                body_stmts.push(formatter_call(
                    "write_newline_indent",
                    fmt_local(),
                    None::<(&str, TypeId)>,
                    span,
                    &formatter_fq,
                ));
                let binding_idx = binding_idx.expect("non-unit case must have a payload binding");
                let binding_name = format!("__inspect_alt_{case_name}_{binding_idx}");
                let payload_local = local_expr(binding_idx, &binding_name, *payload_type, span);
                body_stmts.push(inspect_alt_call(
                    payload_local,
                    *payload_type,
                    fmt_local(),
                    trait_env,
                    module_source,
                    tt,
                    span,
                    inspect_alt_trait,
                    inspect_alt_method,
                ));
                body_stmts.push(write_str_stmt(
                    ",",
                    fmt_local(),
                    string_type,
                    ref_string_type,
                    span,
                    &formatter_fq,
                ));
                body_stmts.push(formatter_call(
                    "close_brace",
                    fmt_local(),
                    Some((")", string_type)),
                    span,
                    &formatter_fq,
                ));
                vec![TirPattern::Binding {
                    name: binding_name,
                    local_index: binding_idx,
                    type_id: *payload_type,
                }]
            };
            TirMatchArm {
                pattern: TirPattern::Variant {
                    enum_type: variant_type,
                    variant_name: case_name.clone(),
                    bindings,
                    payload_type: *payload_type,
                },
                guard: None,
                body: TirExpr::new(
                    TirExprKind::Block(TirBlock::new(body_stmts, span)),
                    TypeTable::UNIT,
                    span,
                ),
                span,
            }
        })
        .collect();

    let match_expr = TirExpr::new(
        TirExprKind::Match {
            expr: Box::new(deref_self),
            arms,
        },
        TypeTable::UNIT,
        span,
    );
    vec![TirStmt::new(TirStmtKind::Expr(match_expr), span)]
}

/// Build a `value.inspect_alt(f)` method call statement.
fn inspect_alt_call(
    value: TirExpr,
    value_type: TypeId,
    fmt: TirExpr,
    trait_env: &TraitEnv,
    module_source: &ModuleSource,
    tt: &mut TypeTable,
    span: Span,
    inspect_alt_trait: &str,
    inspect_alt_method: &str,
) -> TirStmt {
    let call = trait_call_on_type(
        value,
        value_type,
        inspect_alt_trait,
        inspect_alt_method,
        TypeTable::UNIT,
        vec![fmt],
        true,
        trait_env,
        module_source,
        tt,
        span,
    );
    TirStmt::new(TirStmtKind::Expr(call), span)
}

/// Build a `f.method_name(text)` or `f.method_name()` call statement on the Formatter.
fn formatter_call(
    method_name: &str,
    fmt: TirExpr,
    text_arg: Option<(impl Into<String>, TypeId)>,
    span: Span,
    formatter_fq: &FqTypeName,
) -> TirStmt {
    let args = match text_arg {
        Some((text, string_type)) => vec![CallArg::new(
            TirExpr::new(TirExprKind::StringLiteral(text.into()), string_type, span),
            false,
        )],
        None => vec![],
    };
    let call = TirExpr::new(
        TirExprKind::method_call(
            Box::new(fmt),
            FunctionRef {
                module_source: ModuleSource::format(),
                name: format!("{formatter_fq}::{method_name}"),
                monomorph_info: None,
                method_info: Some(LocalMethodName::new(
                    formatter_fq.clone(),
                    None,
                    method_name.to_string(),
                )),
            },
            vec![],
            args,
        ),
        TypeTable::UNIT,
        span,
    );
    TirStmt::new(TirStmtKind::Expr(call), span)
}

/// Generate a delegating format-trait fallback function whose body is a single
/// `self.<delegate_method>(f)` call (used by the `DisplayAlt` → `Display`
/// fallback, and the newtype `Display`/`DisplayAlt` transparent delegation).
///
/// The `display_info` and `inspect_info` `LocalMethodName`s determine the exact
/// mangled names. `impl_type_params` is non-empty for generic structs.
fn generate_display_fallback(
    display_info: LocalMethodName,
    inspect_info: LocalMethodName,
    ref_type: TypeId,
    receiver_type: TypeId,
    fmt_type: TypeId,
    trait_env: &TraitEnv,
    module_source: &ModuleSource,
    impl_type_params: Vec<TirTypeParam>,
    tt: &mut TypeTable,
    span: Span,
) -> TirFunction {
    // Resolve the body's home for the delegate target (the receiver
    // type's `Inspect` / `InspectAlt` impl). For an auto-derived
    // Inspect on a newtype, that's the newtype's module; for a
    // parameterized type like `List<FieldValue>`, List's
    // `impl<T: Inspect> Inspect for List<T>` lives in `core:prelude/format`.
    let delegate_trait = inspect_info
        .base_trait_name
        .as_deref()
        .or(inspect_info.trait_name.as_deref())
        .expect("display fallback delegates to a trait method");
    // A blanket-derived delegate — an enum's `Inspect` from the `ReflectEnum`
    // blanket — has no per-type body, so route the call through the blanket.
    // The instance lands in the blanket's module; naming it by the receiver's
    // module would name a body that module never defines.
    let (delegate_module, delegate_monomorph) =
        match crate::synthesis::template::blanket_dispatch_for(
            trait_env,
            receiver_type,
            delegate_trait,
            &inspect_info.method_name,
            tt,
        ) {
            Some((mono, blanket_module)) => (blanket_module, Some(mono)),
            None => (
                resolve_impl_module_via_env(
                    receiver_type,
                    delegate_trait,
                    tt,
                    trait_env,
                    module_source,
                ),
                None,
            ),
        };
    let qualified_name = display_info.to_mangled_name();

    let params = vec![
        self_param(ref_type, span),
        TirParam {
            name: "f".to_string(),
            type_id: fmt_type,
            local_index: 1,
            is_mut: false,
            is_mut_ref: false,
            span,
        },
    ];

    let self_local = TirExpr::new(
        TirExprKind::Local {
            index: 0,
            name: "self".to_string(),
        },
        ref_type,
        span,
    );
    let fmt_local = TirExpr::new(
        TirExprKind::Local {
            index: 1,
            name: "f".to_string(),
        },
        fmt_type,
        span,
    );

    let delegate_name = inspect_info.to_mangled_name();
    let delegate_call = TirExpr::new(
        TirExprKind::method_call(
            Box::new(self_local),
            FunctionRef {
                module_source: delegate_module,
                name: delegate_name,
                monomorph_info: delegate_monomorph,
                method_info: Some(inspect_info),
            },
            vec![],
            vec![CallArg::new(fmt_local, false)],
        ),
        TypeTable::UNIT,
        span,
    );
    let body = TirBlock::new(
        vec![TirStmt::new(TirStmtKind::Expr(delegate_call), span)],
        span,
    );

    TirFunction {
        module_source: ModuleSource::default(),
        name: qualified_name,
        visibility: crate::ast::Visibility::Public,
        is_export: false,
        is_async: false,
        type_params: Vec::new(),
        impl_type_params,
        monomorph_info: None,
        method_info: Some(display_info),
        params,
        return_type: TypeTable::UNIT,
        task_return_type: None,
        effects: Vec::new(),
        stores: vec![],
        body: Some(body),
        span,
        local_count: 2,
        locals: vec![
            param_local("self", ref_type, false),
            param_local("f", fmt_type, false),
        ],
        address_taken_locals: IndexSet::default(),
        stores_aliased_locals: IndexSet::default(),
        is_cm_binding: false,
        is_dispatch_wrapper: false,
        is_cm_export: false,
        is_ambient: false,
        benign_effects: Vec::new(),
        inline_hint: InlineHint::Auto,
        compiler_item: None,
        export_name: None,
        allocator_tag: None,
        kind: FunctionKind::Regular,

        return_abi: crate::tir::ReturnAbi::default(),
    }
}

/// Generate `DisplayAlt::fmt_alt` fallback implementations that delegate to `Display::fmt`.
fn generate_display_alt_fallback_impls(module: &mut TirModule, ctx: &mut SynthesisCtx<'_, '_, '_>) {
    let pair = TraitPair::display_alt(ctx.names);
    generate_fallback_impls(module, ctx, &pair);
}

/// Walk every type kind in a module and emit a delegating fallback method
/// for the configured `TraitPair`. Skips any type where the target trait is
/// already implemented or the delegate trait is missing.
fn generate_fallback_impls(
    module: &mut TirModule,
    ctx: &mut SynthesisCtx<'_, '_, '_>,
    pair: &TraitPair,
) {
    let module_source = module.module_source.clone();
    let is_display_pair = pair.target_trait == ctx.names.display;
    let all_fn_names: IndexSet<String> = module
        .functions
        .iter()
        .filter_map(|f| f.try_borrow().ok().map(|func| func.name.clone()))
        .collect();
    let mut generated = Vec::new();

    let span = synth_span();
    let (string_name, list_name) = {
        let tt = module.type_table.borrow();
        let items = tt.compiler_items();
        (
            items
                .struct_name(crate::compiler_item::CompilerItem::String)
                .to_string(),
            items
                .struct_name(crate::compiler_item::CompilerItem::List)
                .to_string(),
        )
    };
    let formatter_struct_name = {
        let tt = module.type_table.borrow();
        tt.compiler_struct_name(CompilerItem::Formatter).to_string()
    };
    let mut tt = module.type_table.borrow_mut();
    let formatter_type = tt.make_struct(formatter_struct_name.clone(), ModuleSource::format());
    let fmt_type = tt.make_mut_ref(formatter_type);

    let needs_fallback = |name: &str, ctx: &SynthesisCtx<'_, '_, '_>| -> bool {
        if ctx.has_methodful_impl_anywhere(name, &pair.target_trait) {
            return false;
        }
        if ctx.has_impl(name, &pair.delegate_trait) {
            return true;
        }
        let delegate_key = MethodName::format_local(
            &FqTypeName::declared(&module_source, name),
            Some(&pair.delegate_trait),
            &pair.delegate_method,
        );
        all_fn_names.contains(&delegate_key)
    };

    // Helper to materialise the fallback function. Returns the new function
    // alongside the `(type_name, trait_name)` pair so the caller can record
    // the impl into `ctx` after pushing.
    let make_fallback = |name: &str,
                         ref_type: TypeId,
                         receiver_type: TypeId,
                         impl_type_params: Vec<TirTypeParam>,
                         tt: &mut TypeTable|
     -> Rc<RefCell<TirFunction>> {
        let target_info = trait_method_info(
            &module_source,
            name,
            &pair.target_trait,
            &pair.target_method,
        );
        let delegate_info = trait_method_info(
            &module_source,
            name,
            &pair.delegate_trait,
            &pair.delegate_method,
        );
        Rc::new(RefCell::new(generate_display_fallback(
            target_info,
            delegate_info,
            ref_type,
            receiver_type,
            fmt_type,
            ctx.trait_env,
            &module_source,
            impl_type_params,
            tt,
            span,
        )))
    };

    let enum_names: Vec<_> = module.enums.iter().map(|e| e.name.clone()).collect();
    for name in &enum_names {
        if !needs_fallback(name, ctx) {
            continue;
        }
        let enum_type = tt.make_enum(name.clone(), module_source.clone());
        let ref_type = tt.make_ref(enum_type);
        generated.push(make_fallback(name, ref_type, enum_type, vec![], &mut tt));
        ctx.record_impl(name, &pair.target_trait);
    }

    let struct_names: Vec<_> = module
        .structs
        .iter()
        .filter(|s| s.type_params.is_empty() && s.monomorph_info.is_none())
        .map(|s| s.name.clone())
        .collect();
    for name in &struct_names {
        if name == &string_name || name == &formatter_struct_name {
            continue;
        }
        if !needs_fallback(name, ctx) {
            continue;
        }
        let struct_type = tt.make_struct(name.clone(), module_source.clone());
        let ref_type = tt.make_ref(struct_type);
        generated.push(make_fallback(name, ref_type, struct_type, vec![], &mut tt));
        ctx.record_impl(name, &pair.target_trait);
    }

    let generic_struct_infos: Vec<_> = module
        .structs
        .iter()
        .filter(|s| !s.type_params.is_empty() && s.monomorph_info.is_none())
        .map(|s| (s.name.clone(), s.type_params.clone()))
        .collect();
    for (name, type_params) in &generic_struct_infos {
        if name == &list_name {
            continue;
        }
        if !needs_fallback(name, ctx) {
            continue;
        }
        let type_param_ids = make_type_param_ids(type_params, &mut tt);
        let struct_type =
            tt.make_generic_instance(name.clone(), module_source.clone(), type_param_ids);
        let ref_type = tt.make_ref(struct_type);
        generated.push(make_fallback(
            name,
            ref_type,
            struct_type,
            type_params.clone(),
            &mut tt,
        ));
        ctx.record_impl(name, &pair.target_trait);
    }

    let variant_names: Vec<_> = module
        .variants
        .iter()
        .filter(|v| v.type_params.is_empty())
        .map(|v| v.name.clone())
        .collect();
    for name in &variant_names {
        if !needs_fallback(name, ctx) {
            continue;
        }
        let variant_type = tt.make_variant(name.clone(), module_source.clone());
        let ref_type = tt.make_ref(variant_type);
        generated.push(make_fallback(name, ref_type, variant_type, vec![], &mut tt));
        ctx.record_impl(name, &pair.target_trait);
    }

    let generic_variant_infos: Vec<_> = module
        .variants
        .iter()
        .filter(|v| !v.type_params.is_empty())
        .map(|v| (v.name.clone(), v.type_params.clone()))
        .collect();
    for (name, type_params) in &generic_variant_infos {
        if !needs_fallback(name, ctx) {
            continue;
        }
        let type_param_ids = make_type_param_ids(type_params, &mut tt);
        let variant_type =
            tt.make_generic_instance(name.clone(), module_source.clone(), type_param_ids);
        let ref_type = tt.make_ref(variant_type);
        generated.push(make_fallback(
            name,
            ref_type,
            variant_type,
            type_params.clone(),
            &mut tt,
        ));
        ctx.record_impl(name, &pair.target_trait);
    }

    let flags_infos: Vec<_> = module
        .flags
        .iter()
        .map(|f| (f.name.clone(), f.type_id))
        .collect();
    for (name, flags_type_id) in &flags_infos {
        if !needs_fallback(name, ctx) {
            continue;
        }
        let ref_type = tt.make_ref(*flags_type_id);
        generated.push(make_fallback(
            name,
            ref_type,
            *flags_type_id,
            vec![],
            &mut tt,
        ));
        ctx.record_impl(name, &pair.target_trait);
    }

    for nt in &module.newtypes {
        if module.flags.iter().any(|f| f.type_id == nt.type_id) {
            continue;
        }
        if !needs_fallback(&nt.name, ctx) {
            continue;
        }
        let ref_type = tt.make_ref(nt.type_id);
        if is_display_pair {
            // A newtype's `Display` is transparent: it delegates to the base
            // type's `Display` with no ` as Name` suffix (unlike `Inspect`).
            // `DisplayAlt` reaches the same base display by chaining through
            // this via the `DisplayAlt → Display` delegate.
            let ResolvedType::Newtype { base_type, .. } = tt.get(nt.type_id) else {
                unreachable!("module.newtypes entry {} is not a Newtype type", nt.name);
            };
            let base_type = *base_type;
            generated.push(Rc::new(RefCell::new(generate_newtype_fmt_fn(
                &nt.name,
                nt.type_id,
                base_type,
                ref_type,
                fmt_type,
                ctx.trait_env,
                &module_source,
                &mut tt,
                span,
                &pair.target_trait,
                &pair.target_method,
                None,
            ))));
        } else {
            generated.push(make_fallback(
                &nt.name,
                ref_type,
                nt.type_id,
                vec![],
                &mut tt,
            ));
        }
        ctx.record_impl(&nt.name, &pair.target_trait);
    }

    // Parameterized types (opaque types). Tuples are skipped because their
    // fallback is provided by a variadic impl in `core:prelude/tuple.wado`.
    // `Fn` signatures are handled separately below.
    for (type_id, base_name, type_arg_names) in collect_parameterized_types(&tt) {
        // `collect_parameterized_types` returns only the base name without a
        // module source, so `TypeTable::is_tuple_type` is unavailable here. The
        // `TUPLE_TYPE_NAME` check is sound because `collect_parameterized_types`
        // only emits that name for actual tuple types.
        if base_name == TypeTable::TUPLE_TYPE_NAME {
            continue;
        }
        let mangled = tt.fq_type_name(type_id).to_mangled();
        if ctx.has_methodful_impl_anywhere(&mangled, &pair.target_trait) {
            continue;
        }
        let delegate_present = ctx.has_impl(&mangled, &pair.delegate_trait) || {
            let delegate_key = format!(
                "{mangled}^{}::{}",
                pair.delegate_trait, pair.delegate_method
            );
            all_fn_names.contains(&delegate_key)
        };
        if !delegate_present {
            continue;
        }
        let ref_type = tt.make_ref(type_id);
        let target_info = trait_method_info(
            &module_source,
            &base_name,
            &pair.target_trait,
            &pair.target_method,
        )
        .with_struct_type_args(&type_arg_names);
        let delegate_info = trait_method_info(
            &module_source,
            &base_name,
            &pair.delegate_trait,
            &pair.delegate_method,
        )
        .with_struct_type_args(&type_arg_names);
        generated.push(Rc::new(RefCell::new(generate_display_fallback(
            target_info,
            delegate_info,
            ref_type,
            type_id,
            fmt_type,
            ctx.trait_env,
            &module_source,
            vec![],
            &mut tt,
            span,
        ))));
        // Per-module: do not `ctx.record_impl`.
    }

    // `Fn` dispatch-stub fallbacks — one per canonical `(arity, return_type)`.
    for sig in collect_canonical_fn_signatures(&tt) {
        let mangled = sig.receiver().to_mangled();
        if ctx.has_methodful_impl_anywhere(&mangled, &pair.target_trait) {
            continue;
        }
        let delegate_present = ctx.has_impl(&mangled, &pair.delegate_trait) || {
            let delegate_key = format!(
                "{mangled}^{}::{}",
                pair.delegate_trait, pair.delegate_method
            );
            all_fn_names.contains(&delegate_key)
        };
        if !delegate_present {
            continue;
        }
        let ref_type = tt.make_ref(sig.repr_type_id);
        let target_info = trait_method_info(
            &module_source,
            crate::name::CLOSURE_FN_TRAIT,
            &pair.target_trait,
            &pair.target_method,
        )
        .with_struct_type_args(&sig.type_arg_names);
        let delegate_info = trait_method_info(
            &module_source,
            crate::name::CLOSURE_FN_TRAIT,
            &pair.delegate_trait,
            &pair.delegate_method,
        )
        .with_struct_type_args(&sig.type_arg_names);
        generated.push(Rc::new(RefCell::new(generate_display_fallback(
            target_info,
            delegate_info,
            ref_type,
            sig.repr_type_id,
            fmt_type,
            ctx.trait_env,
            &module_source,
            vec![],
            &mut tt,
            span,
        ))));
        // Per-module: do not `ctx.record_impl`.
    }

    drop(tt);
    module.functions.extend(generated);
}

/// Build a `value.inspect(f)` method call statement.
fn inspect_call(
    value: TirExpr,
    value_type: TypeId,
    fmt: TirExpr,
    trait_env: &TraitEnv,
    module_source: &ModuleSource,
    tt: &mut TypeTable,
    span: Span,
    inspect_trait: &str,
    inspect_method: &str,
) -> TirStmt {
    let call = trait_call_on_type(
        value,
        value_type,
        inspect_trait,
        inspect_method,
        TypeTable::UNIT,
        vec![fmt],
        true,
        trait_env,
        module_source,
        tt,
        span,
    );
    TirStmt::new(TirStmtKind::Expr(call), span)
}

/// Decompose a type into `(base_name, is_type_param, type_arg_names)` for `LocalMethodName`.
///
/// All parameterized types are explicitly handled to ensure the base name never
/// contains `<`, which would cause `LocalMethodName::new` to panic.
fn decompose_type_for_method_name(
    resolved: &ResolvedType,
    type_id: TypeId,
    tt: &TypeTable,
) -> (Receiver, bool, Vec<FqTypeName>) {
    match resolved {
        ResolvedType::Function {
            params,
            return_type,
            ..
        } => (
            Receiver::Type(FqTypeName::builtin(crate::name::CLOSURE_FN_TRAIT)),
            false,
            crate::name::fn_type_args(params.len(), &tt.fq_type_name(*return_type)),
        ),
        ResolvedType::Reactive(inner) => (
            Receiver::Type(FqTypeName::builtin("Reactive")),
            false,
            vec![tt.fq_type_name(*inner)],
        ),
        ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => (
            Receiver::Ref(RefKind::from_resolved(resolved).expect("ref classify")),
            false,
            vec![tt.fq_type_name(*inner)],
        ),
        // Everything else is already the structured shape the type table
        // reports: the head names the receiver, its arguments are the method
        // name's type args.
        _ => {
            let fq = tt.fq_type_name(type_id);
            let args = fq.args().to_vec();
            (
                Receiver::Type(fq.head_only()),
                matches!(resolved, ResolvedType::TypeParam { .. }),
                args,
            )
        }
    }
}

/// Determine the module where an Inspect impl lives for a given type.
/// Determine the module where a trait impl lives for a given type.
///
/// `ref_module` is used for Ref/MutRef types (`traits()` for Eq/Ord, `format()` for Inspect).
/// `string_module` is used for String (`string()` for Eq/Ord, `format()` for Inspect).
/// Resolve `module_source` for a `value.<trait>::<method>` call inside an
/// auto-derived body — issue #1110 (1): the `FunctionRef`'s `module_source`
/// must be the module that hosts the callee's body.
///
/// Strategy:
///   1. Ask `TraitEnv` where `impl <trait> for <receiver-type>` lives.
///      `TraitEnv` indexes every AST-layer impl block by struct name and
///      trait name, so this hits for cross-module impls
///      (`impl Display for String` in `core:prelude/format`,
///      `impl<..T> Inspect for [..T]` in `core:prelude/tuple`, ref/mutref
///      blankets, etc.). This is a deterministic resolution, not a
///      fall-back to whichever module the synthesis pass happens to be
///      visiting.
///   2. If `TraitEnv` is silent — the impl is being auto-derived in the
///      current synthesis pass and hasn't been published to the
///      synthesised layer yet — fall back to the receiver type's own
///      module. `synthesis::traits::generate_*_impls` places auto-
///      derived bodies in the receiver type's module by convention, so
///      that's where the body will live.
///   3. For receivers with no defining module (`TypeParam`, unresolved),
///      use the caller-supplied `fallback` (the current synthesis module).
///      The fallback only applies to producer outputs that the
///      monomorphizer immediately overwrites with a concrete-type
///      module after type-param substitution.
fn resolve_impl_module_via_env(
    type_id: TypeId,
    trait_name: &str,
    tt: &TypeTable,
    trait_env: &TraitEnv,
    fallback: &ModuleSource,
) -> ModuleSource {
    let resolved = tt.get(type_id).clone();

    let candidate_name: Option<String> = match &resolved {
        ResolvedType::Ref(_) | ResolvedType::MutRef(_) => {
            RefKind::from_resolved(&resolved).map(|k| k.prefix().to_string())
        }
        ResolvedType::Primitive(p) => Some(p.as_str().to_string()),
        ResolvedType::Unit => Some(TypeTable::UNIT_TYPE_NAME.to_string()),
        ResolvedType::Struct { name, .. }
        | ResolvedType::Enum { name, .. }
        | ResolvedType::Variant { name, .. }
        | ResolvedType::Newtype { name, .. }
        | ResolvedType::Flags { name, .. }
        | ResolvedType::GenericInstance { name, .. }
        | ResolvedType::GenericResource { name, .. } => Some(name.clone().to_string()),
        ResolvedType::BuiltinArray(_) => Some(TypeTable::ARRAY_TYPE_NAME.to_string()),
        _ => None,
    };

    let type_module: Option<ModuleSource> = match &resolved {
        ResolvedType::Struct { module_source, .. }
        | ResolvedType::Enum { module_source, .. }
        | ResolvedType::Variant { module_source, .. }
        | ResolvedType::Newtype { module_source, .. }
        | ResolvedType::Flags { module_source, .. }
        | ResolvedType::GenericInstance { module_source, .. }
        | ResolvedType::GenericResource { module_source, .. } => Some(module_source.clone()),
        ResolvedType::Primitive(_) | ResolvedType::Unit => Some(ModuleSource::primitive()),
        ResolvedType::BuiltinArray(_) => Some(ModuleSource::array()),
        _ => None,
    };

    if let Some(name) = candidate_name.as_deref()
        && let Some(m) = trait_env.impl_module_for(name, trait_name, type_module.as_ref())
    {
        return m.clone();
    }
    type_module.unwrap_or_else(|| fallback.clone())
}

/// Collect parameterized types that need Inspect/Display impls — per-`TypeId`
/// kinds whose codegen genuinely depends on the distinct `TypeId`.
///
/// Returns `(type_id, base_name, type_arg_names)` for each concrete
/// parameterized type. Includes tuples and resource handle types (Future,
/// Stream, etc.).
///
/// `ResolvedType::Function` is intentionally **not** enumerated here.
/// `Fn` dispatch stubs depend only on `(arity, return_type)` because
/// `wir_build::build_fn_canonical_dispatch_body` casts `self` to the shared
/// `canonical_inspectable_base` before reading the vtable, so the per-
/// parameter `TypeId`s are irrelevant at codegen. Enumerating Function
/// types per `TypeId` here would emit one identical stub per `TypeId` and
/// collide on `function_id_for`, which is required to be injective over
/// `project.functions` (asserted in `optimize/dce`). Use
/// [`collect_canonical_fn_signatures`] for the `(arity, return_type)` view.
fn collect_parameterized_types(tt: &TypeTable) -> Vec<(TypeId, String, Vec<FqTypeName>)> {
    let is_concrete = |t: TypeId| !matches!(tt.get(t), ResolvedType::TypeParam { .. });

    tt.all_types()
        .filter_map(|(id, resolved)| match resolved {
            ResolvedType::GenericInstance {
                name,
                type_args,
                module_source,
            } if TypeTable::is_tuple_type(name) => {
                if !type_args.iter().all(|e| is_concrete(*e)) {
                    return None;
                }
                let args = type_args.iter().map(|e| tt.fq_type_name(*e)).collect();
                Some((id, TypeTable::TUPLE_TYPE_NAME.to_string(), args))
            }
            ResolvedType::GenericResource {
                name, type_args, ..
            } => {
                if !type_args.iter().all(|t| is_concrete(*t)) {
                    return None;
                }
                let args = type_args.iter().map(|t| tt.fq_type_name(*t)).collect();
                Some((id, name.clone(), args))
            }
            _ => None,
        })
        .collect()
}

/// Canonical `Fn` signature for dispatch-stub synthesis.
///
/// `Fn` dispatch stubs (`Fn<arity, ret>^Inspect::inspect`,
/// `Fn<arity, ret>^InspectAlt::inspect_alt`, and fallbacks) are keyed by
/// `(arity, return_type)` alone — see `collect_parameterized_types` for the
/// rationale. `repr_type_id` is the first encountered `ResolvedType::Function`
/// `TypeId` with this signature; synthesis uses it to build the stub's `&self`
/// type via `tt.make_ref(repr_type_id)`. Any `TypeId` with the same signature
/// would work — the choice is deterministic-by-iteration-order so two
/// compiles produce byte-identical output.
struct FnSignature {
    repr_type_id: TypeId,
    arity: usize,
    return_type: TypeId,
    /// `[arity, return_type]` (see [`crate::name::fn_type_args`]) — the form
    /// consumed by the dispatch-stub emitters.
    type_arg_names: Vec<FqTypeName>,
}

impl FnSignature {
    /// The `Fn<arity,ret>` receiver this signature's dispatch stubs hang off.
    fn receiver(&self) -> FqTypeName {
        FqTypeName::builtin(crate::name::CLOSURE_FN_TRAIT).with_args(self.type_arg_names.clone())
    }
}

fn collect_canonical_fn_signatures(tt: &TypeTable) -> Vec<FnSignature> {
    let is_concrete = |t: TypeId| !matches!(tt.get(t), ResolvedType::TypeParam { .. });
    // Dedup by mangled name, not return-type `TypeId`: `&T` / `&mut T` mangle
    // identically and must share one stub, else the stubs collide post-mono.
    let mut seen: IndexSet<(usize, String)> = IndexSet::default();
    let mut result = Vec::new();

    for (id, resolved) in tt.all_types() {
        let ResolvedType::Function {
            params,
            return_type,
            ..
        } = resolved
        else {
            continue;
        };
        if !params.iter().all(|p| is_concrete(*p)) || !is_concrete(*return_type) {
            continue;
        }
        let arity = params.len();
        let return_type_fq = tt.fq_type_name(*return_type);
        if !seen.insert((arity, return_type_fq.to_mangled())) {
            continue;
        }
        result.push(FnSignature {
            repr_type_id: id,
            arity,
            return_type: *return_type,
            type_arg_names: crate::name::fn_type_args(arity, &return_type_fq),
        });
    }
    result
}

/// Generate `EnumName^Eq::eq(&self, &Self) -> bool`
///
/// Body: `return *self == *other;` (i32 comparison via enum discriminant)
fn generate_enum_eq_fn(
    module_source: &ModuleSource,
    enum_name: &str,
    enum_type: TypeId,
    ref_enum_type: TypeId,
    eq_trait_name: &str,
    span: Span,
) -> TirFunction {
    let method_info = trait_method_info(module_source, enum_name, eq_trait_name, "eq");
    let qualified_name = method_info.to_mangled_name();

    let comparison = TirExpr::new(
        TirExprKind::Binary {
            left: Box::new(deref_local(0, "self", ref_enum_type, enum_type, span)),
            op: TirBinaryOp::Eq,
            right: Box::new(deref_local(1, "other", ref_enum_type, enum_type, span)),
        },
        TypeTable::BOOL,
        span,
    );
    let body = TirBlock::new(
        vec![TirStmt::new(
            TirStmtKind::Return {
                value: Some(comparison),
            },
            span,
        )],
        span,
    );

    make_synthetic_method(
        qualified_name,
        method_info,
        binary_method_params(ref_enum_type, span),
        TypeTable::BOOL,
        body,
        binary_method_locals(ref_enum_type),
    )
}

/// Generate `EnumName^Ord::cmp(&self, &Self) -> Ordering`
///
/// Body:
/// ```text
/// let a = *self;
/// let b = *other;
/// if a < b { return Ordering::Less; }
/// if a > b { return Ordering::Greater; }
/// return Ordering::Equal;
/// ```
fn generate_enum_ord_fn(
    module_source: &ModuleSource,
    enum_name: &str,
    enum_type: TypeId,
    ref_enum_type: TypeId,
    ordering_type: TypeId,
    ord_trait_name: &str,
    span: Span,
    names: &TraitsStdlibNames,
) -> TirFunction {
    let method_info = trait_method_info(module_source, enum_name, ord_trait_name, "cmp");
    let qualified_name = method_info.to_mangled_name();

    let local_a = || local_expr(2, "a", enum_type, span);
    let local_b = || local_expr(3, "b", enum_type, span);

    let cmp_branch = |op, ordering_case_index, ordering_case_name: &str| {
        let cond = TirExpr::new(
            TirExprKind::Binary {
                left: Box::new(local_a()),
                op,
                right: Box::new(local_b()),
            },
            TypeTable::BOOL,
            span,
        );
        TirStmt::new(
            TirStmtKind::If {
                condition: cond,
                then_block: TirBlock::new(
                    vec![TirStmt::new(
                        TirStmtKind::Return {
                            value: Some(ordering_construct(
                                ordering_type,
                                ordering_case_index,
                                ordering_case_name,
                                span,
                            )),
                        },
                        span,
                    )],
                    span,
                ),
                else_block: None,
            },
            span,
        )
    };

    let let_local = |name, local_index, value| {
        TirStmt::new(
            TirStmtKind::Let {
                name: String::from(name),
                local_index,
                is_mut: false,
                is_reactive: false,
                type_id: enum_type,
                value,
                skip_value_copy: false,
            },
            span,
        )
    };

    let body = TirBlock::new(
        vec![
            let_local(
                "a",
                2,
                deref_local(0, "self", ref_enum_type, enum_type, span),
            ),
            let_local(
                "b",
                3,
                deref_local(1, "other", ref_enum_type, enum_type, span),
            ),
            cmp_branch(TirBinaryOp::Lt, names.less_index, &names.less_name),
            cmp_branch(TirBinaryOp::Gt, names.greater_index, &names.greater_name),
            TirStmt::new(
                TirStmtKind::Return {
                    value: Some(ordering_construct(
                        ordering_type,
                        names.equal_index,
                        &names.equal_name,
                        span,
                    )),
                },
                span,
            ),
        ],
        span,
    );

    let mut locals = binary_method_locals(ref_enum_type);
    locals.push(TirLocal::synth(2, enum_type, false));
    locals.push(TirLocal::synth(3, enum_type, false));

    make_synthetic_method(
        qualified_name,
        method_info,
        binary_method_params(ref_enum_type, span),
        ordering_type,
        body,
        locals,
    )
}

/// Build a trait method call on a value: `value.Trait::method(args...)`.
///
/// Handles type decomposition, method name mangling, impl module resolution,
/// and Ref/MutRef monomorphization. The value is automatically wrapped in a reference.
fn trait_call_on_type(
    value: TirExpr,
    value_type: TypeId,
    trait_name: &str,
    method_name: &str,
    return_type: TypeId,
    args: Vec<TirExpr>,
    needs_ref_monomorph: bool,
    trait_env: &TraitEnv,
    module_source: &ModuleSource,
    tt: &mut TypeTable,
    span: Span,
) -> TirExpr {
    let ref_type = tt.make_ref(value_type);
    let receiver = ref_expr(value, ref_type, span);

    let resolved = tt.get(value_type).clone();
    let (recv, is_type_param, type_arg_names) =
        decompose_type_for_method_name(&resolved, value_type, tt);

    let mut info = LocalMethodName::of(recv, Some(trait_name.to_string()), method_name.to_string());
    if !type_arg_names.is_empty() {
        info = info.with_struct_type_args(&type_arg_names);
    }
    info.is_type_param_receiver = is_type_param;

    // For `T::method` where `T` is a type parameter, the body's home
    // module isn't known until monomorphization substitutes `T` with a
    // concrete type. `module_source` (the surrounding synthesis module)
    // is a placeholder; `resolve_method_call_substitution` rewrites it
    // to the concrete impl's module once `T` is resolved.
    // A blanket-derived callee (e.g. a `ReflectStruct`-derived struct's `Inspect`)
    // has no per-type body; route the call through the blanket so the
    // monomorphizer instantiates it, rather than emitting an unresolved
    // `Struct^Inspect::inspect` the WIR-build trait-bound check rejects.
    let blanket = if is_type_param {
        None
    } else {
        crate::synthesis::template::blanket_dispatch_for(
            trait_env,
            value_type,
            trait_name,
            method_name,
            tt,
        )
    };

    let (impl_module, monomorph_info) = if let Some((mono, blanket_module)) = blanket {
        (blanket_module, Some(mono))
    } else {
        let impl_module = if is_type_param {
            module_source.clone()
        } else {
            resolve_impl_module_via_env(value_type, trait_name, tt, trait_env, module_source)
        };
        let monomorph_info = if needs_ref_monomorph {
            match &resolved {
                ResolvedType::Ref(inner_id) | ResolvedType::MutRef(inner_id) => {
                    let base_info = trait_method_info(
                        module_source,
                        &info.base_struct_name(),
                        trait_name,
                        method_name,
                    );
                    Some(MonomorphInfo {
                        generic_name: base_info.to_mangled_name(),
                        impl_type_args: vec![*inner_id],
                        method_type_args: vec![],
                        is_blanket: true,
                    })
                }
                _ => None,
            }
        } else {
            None
        };
        (impl_module, monomorph_info)
    };

    let fn_name = info.to_mangled_name();
    TirExpr::new(
        TirExprKind::method_call(
            Box::new(receiver),
            FunctionRef {
                module_source: impl_module,
                name: fn_name,
                monomorph_info,
                method_info: Some(info),
            },
            vec![],
            args.into_iter().map(|e| CallArg::new(e, false)).collect(),
        ),
        return_type,
        span,
    )
}

/// Build an `Eq::eq` method call on a field value: `self.field.eq(&other.field)`.
fn eq_call_expr(
    self_field: TirExpr,
    other_field: TirExpr,
    field_type: TypeId,
    trait_env: &TraitEnv,
    module_source: &ModuleSource,
    tt: &mut TypeTable,
    span: Span,
) -> TirExpr {
    let ref_type = tt.make_ref(field_type);
    let arg = ref_expr(other_field, ref_type, span);
    let eq_trait_name = tt
        .compiler_trait_name(crate::compiler_item::CompilerItem::Eq)
        .to_string();
    trait_call_on_type(
        self_field,
        field_type,
        &eq_trait_name,
        "eq",
        TypeTable::BOOL,
        vec![arg],
        true,
        trait_env,
        module_source,
        tt,
        span,
    )
}

/// Build an `Ord::cmp` method call on a field value: `self.field.cmp(&other.field)`.
fn cmp_call_expr(
    self_field: TirExpr,
    other_field: TirExpr,
    field_type: TypeId,
    ordering_type: TypeId,
    trait_env: &TraitEnv,
    module_source: &ModuleSource,
    tt: &mut TypeTable,
    span: Span,
) -> TirExpr {
    let ref_type = tt.make_ref(field_type);
    let arg = ref_expr(other_field, ref_type, span);
    let ord_trait_name = tt
        .compiler_trait_name(crate::compiler_item::CompilerItem::Ord)
        .to_string();
    trait_call_on_type(
        self_field,
        field_type,
        &ord_trait_name,
        "cmp",
        ordering_type,
        vec![arg],
        false,
        trait_env,
        module_source,
        tt,
        span,
    )
}

/// Generate `StructName^Eq::eq(&self, &Self) -> bool` for non-generic structs.
///
/// Body: `return self.f0 == other.f0 && self.f1 == other.f1 && ...`
/// Empty structs: `return true;`
fn generate_struct_eq_fn(
    struct_name: &str,
    impl_type_params: &[TirTypeParam],
    fields: &[(String, TypeId, u32)],
    ref_struct_type: TypeId,
    trait_env: &TraitEnv,
    module_source: &ModuleSource,
    eq_trait_name: &str,
    tt: &mut TypeTable,
    span: Span,
) -> TirFunction {
    let method_info = trait_method_info(module_source, struct_name, eq_trait_name, "eq");
    let qualified_name = method_info.to_mangled_name();

    let result = build_struct_eq_chain(fields, ref_struct_type, trait_env, module_source, tt, span);
    let body = TirBlock::new(
        vec![TirStmt::new(
            TirStmtKind::Return {
                value: Some(result),
            },
            span,
        )],
        span,
    );

    make_trait_method(
        qualified_name,
        method_info,
        impl_type_params.to_vec(),
        binary_method_params(ref_struct_type, span),
        TypeTable::BOOL,
        body,
        binary_method_locals(ref_struct_type),
        span,
    )
}

/// Build the AND-chain `self.f0.eq(&other.f0) && self.f1.eq(&other.f1) && ...`
/// for a struct's `Eq::eq` body. Returns `true` for an empty field list.
fn build_struct_eq_chain(
    fields: &[(String, TypeId, u32)],
    ref_struct_type: TypeId,
    trait_env: &TraitEnv,
    module_source: &ModuleSource,
    tt: &mut TypeTable,
    span: Span,
) -> TirExpr {
    if fields.is_empty() {
        return TirExpr::new(TirExprKind::BoolLiteral(true), TypeTable::BOOL, span);
    }

    let field_eq = |name: &str, field_type: TypeId, field_index: u32, tt: &mut TypeTable| {
        let self_field = field_access_local(
            0,
            "self",
            ref_struct_type,
            field_index,
            name,
            field_type,
            span,
        );
        let other_field = field_access_local(
            1,
            "other",
            ref_struct_type,
            field_index,
            name,
            field_type,
            span,
        );
        eq_call_expr(
            self_field,
            other_field,
            field_type,
            trait_env,
            module_source,
            tt,
            span,
        )
    };

    let mut iter = fields.iter();
    let (first_name, first_type, first_index) = iter.next().unwrap();
    let mut result = field_eq(first_name, *first_type, *first_index, tt);
    for (field_name, field_type, field_index) in iter {
        let cmp = field_eq(field_name, *field_type, *field_index, tt);
        result = TirExpr::new(
            TirExprKind::Binary {
                op: TirBinaryOp::And,
                left: Box::new(result),
                right: Box::new(cmp),
            },
            TypeTable::BOOL,
            span,
        );
    }
    result
}

/// Build `<local>.<field>` where `<local>` is a local of reference type.
fn field_access_local(
    local_index: u32,
    local_name: &str,
    local_ref_type: TypeId,
    field_index: u32,
    field_name: &str,
    field_type: TypeId,
    span: Span,
) -> TirExpr {
    TirExpr::new(
        TirExprKind::FieldAccess {
            expr: Box::new(local_expr(local_index, local_name, local_ref_type, span)),
            field_index,
            field_name: field_name.to_string(),
        },
        field_type,
        span,
    )
}

/// Generate `StructName^Ord::cmp(&self, &Self) -> Ordering` for non-generic structs.
///
/// Body (lexicographic):
/// ```text
/// let c = self.f0.cmp(&other.f0);
/// if c != Ordering::Equal { return c; }
/// let c = self.f1.cmp(&other.f1);
/// if c != Ordering::Equal { return c; }
/// ...
/// return Ordering::Equal;
/// ```
fn generate_struct_ord_fn(
    struct_name: &str,
    impl_type_params: &[TirTypeParam],
    fields: &[(String, TypeId, u32)],
    ref_struct_type: TypeId,
    ordering_type: TypeId,
    trait_env: &TraitEnv,
    module_source: &ModuleSource,
    ord_trait_name: &str,
    tt: &mut TypeTable,
    span: Span,
    names: &TraitsStdlibNames,
) -> TirFunction {
    let method_info = trait_method_info(module_source, struct_name, ord_trait_name, "cmp");
    let qualified_name = method_info.to_mangled_name();

    let (stmts, locals) = build_struct_ord_body(
        fields,
        ref_struct_type,
        ordering_type,
        trait_env,
        module_source,
        tt,
        span,
        names,
    );
    let body = TirBlock::new(stmts, span);

    make_trait_method(
        qualified_name,
        method_info,
        impl_type_params.to_vec(),
        binary_method_params(ref_struct_type, span),
        ordering_type,
        body,
        locals,
        span,
    )
}

/// Build the lexicographic compare statements for a struct `Ord::cmp` body.
///
/// Each field gets a `let c_i = self.fi.cmp(&other.fi);` followed by
/// `if c_i != Ordering::Equal { return c_i; }`, terminated by
/// `return Ordering::Equal;`. Returns the statement list and the locals
/// table (which includes the `c_i` slots for each field, starting at
/// local index 2).
fn build_struct_ord_body(
    fields: &[(String, TypeId, u32)],
    ref_struct_type: TypeId,
    ordering_type: TypeId,
    trait_env: &TraitEnv,
    module_source: &ModuleSource,
    tt: &mut TypeTable,
    span: Span,
    names: &TraitsStdlibNames,
) -> (Vec<TirStmt>, Vec<TirLocal>) {
    let mut stmts = Vec::new();
    let mut locals = binary_method_locals(ref_struct_type);

    for (local_idx, (field_name, field_type, field_index)) in (2_u32..).zip(fields.iter()) {
        let self_field = field_access_local(
            0,
            "self",
            ref_struct_type,
            *field_index,
            field_name,
            *field_type,
            span,
        );
        let other_field = field_access_local(
            1,
            "other",
            ref_struct_type,
            *field_index,
            field_name,
            *field_type,
            span,
        );
        let cmp_result = cmp_call_expr(
            self_field,
            other_field,
            *field_type,
            ordering_type,
            trait_env,
            module_source,
            tt,
            span,
        );

        locals.push(TirLocal {
            name: "c".to_string(),
            type_id: ordering_type,
            is_mut: false,
        });

        stmts.push(TirStmt::new(
            TirStmtKind::Let {
                name: "c".to_string(),
                local_index: local_idx,
                is_mut: false,
                is_reactive: false,
                type_id: ordering_type,
                value: cmp_result,
                skip_value_copy: false,
            },
            span,
        ));

        let local_c = local_expr(local_idx, "c", ordering_type, span);
        let cond = TirExpr::new(
            TirExprKind::Binary {
                op: TirBinaryOp::NotEq,
                left: Box::new(local_c.clone()),
                right: Box::new(ordering_construct(
                    ordering_type,
                    names.equal_index,
                    &names.equal_name,
                    span,
                )),
            },
            TypeTable::BOOL,
            span,
        );
        stmts.push(TirStmt::new(
            TirStmtKind::If {
                condition: cond,
                then_block: TirBlock::new(
                    vec![TirStmt::new(
                        TirStmtKind::Return {
                            value: Some(local_c),
                        },
                        span,
                    )],
                    span,
                ),
                else_block: None,
            },
            span,
        ));
    }

    stmts.push(TirStmt::new(
        TirStmtKind::Return {
            value: Some(ordering_construct(
                ordering_type,
                names.equal_index,
                &names.equal_name,
                span,
            )),
        },
        span,
    ));

    (stmts, locals)
}

/// Generate `VariantName^Eq::eq(&self, &Self) -> bool`.
///
/// Body: if-else chain testing each case with `VariantTest`, comparing payloads via `eq_call_expr`.
/// Pass an empty `impl_type_params` slice for non-generic variants.
fn generate_variant_eq_fn(
    variant_name: &str,
    impl_type_params: &[TirTypeParam],
    cases: &[(String, u32, TypeId)],
    variant_type: TypeId,
    ref_variant_type: TypeId,
    trait_env: &TraitEnv,
    module_source: &ModuleSource,
    tt: &mut TypeTable,
    span: Span,
) -> TirFunction {
    let eq_trait_name = tt
        .compiler_trait_name(crate::compiler_item::CompilerItem::Eq)
        .to_string();
    let method_info = trait_method_info(module_source, variant_name, &eq_trait_name, "eq");
    let qualified_name = method_info.to_mangled_name();

    // Allocate two payload-binding locals (self-side, other-side) for
    // every non-unit case. They follow the two parameter locals (self,
    // other) and feed the nested `TirPattern::Variant { bindings }`
    // arms produced by `variant_eq_body`.
    let mut locals = binary_method_locals(ref_variant_type);
    let mut payload_bindings: Vec<Option<(u32, u32)>> = Vec::with_capacity(cases.len());
    for (case_name, _, payload_type) in cases {
        if *payload_type == TypeTable::UNIT {
            payload_bindings.push(None);
        } else {
            let self_idx = locals.len() as u32;
            locals.push(param_local(
                &format!("__eq_self_{case_name}_{self_idx}"),
                *payload_type,
                false,
            ));
            let other_idx = locals.len() as u32;
            locals.push(param_local(
                &format!("__eq_other_{case_name}_{other_idx}"),
                *payload_type,
                false,
            ));
            payload_bindings.push(Some((self_idx, other_idx)));
        }
    }

    let body_stmts = variant_eq_body(
        cases,
        &payload_bindings,
        variant_type,
        ref_variant_type,
        trait_env,
        module_source,
        tt,
        span,
    );
    let body = TirBlock::new(body_stmts, span);

    make_trait_method(
        qualified_name,
        method_info,
        impl_type_params.to_vec(),
        binary_method_params(ref_variant_type, span),
        TypeTable::BOOL,
        body,
        locals,
        span,
    )
}

/// Build the body statements for variant Eq: a nested TIR `Match` —
/// the outer Match dispatches on `*self`, and each arm runs an inner
/// Match on `*other` to either compare payloads (matching case) or
/// return `false` (mismatched case). Variant Match is exhaustive at
/// TIR level, so no final fallback is needed.
fn variant_eq_body(
    cases: &[(String, u32, TypeId)],
    payload_bindings: &[Option<(u32, u32)>],
    variant_type: TypeId,
    ref_variant_type: TypeId,
    trait_env: &TraitEnv,
    module_source: &ModuleSource,
    tt: &mut TypeTable,
    span: Span,
) -> Vec<TirStmt> {
    if cases.is_empty() {
        return vec![TirStmt::new(
            TirStmtKind::Return {
                value: Some(TirExpr::new(
                    TirExprKind::BoolLiteral(true),
                    TypeTable::BOOL,
                    span,
                )),
            },
            span,
        )];
    }

    let deref_self = deref_local(0, "self", ref_variant_type, variant_type, span);
    let deref_other = || deref_local(1, "other", ref_variant_type, variant_type, span);

    let return_bool = |b: bool| -> TirExpr {
        TirExpr::new(
            TirExprKind::Block(TirBlock::new(
                vec![TirStmt::new(
                    TirStmtKind::Return {
                        value: Some(TirExpr::new(
                            TirExprKind::BoolLiteral(b),
                            TypeTable::BOOL,
                            span,
                        )),
                    },
                    span,
                )],
                span,
            )),
            TypeTable::UNIT,
            span,
        )
    };

    let outer_arms: Vec<TirMatchArm> = cases
        .iter()
        .zip(payload_bindings.iter())
        .map(|((case_name, _, payload_type), binding)| {
            let is_unit = *payload_type == TypeTable::UNIT;
            let (self_bindings, inner_arms): (Vec<TirPattern>, Vec<TirMatchArm>) = if is_unit {
                let inner_arms = vec![
                    TirMatchArm {
                        pattern: TirPattern::Variant {
                            enum_type: variant_type,
                            variant_name: case_name.clone(),
                            bindings: Vec::new(),
                            payload_type: *payload_type,
                        },
                        guard: None,
                        body: return_bool(true),
                        span,
                    },
                    TirMatchArm {
                        pattern: TirPattern::Wildcard,
                        guard: None,
                        body: return_bool(false),
                        span,
                    },
                ];
                (Vec::new(), inner_arms)
            } else {
                let (self_idx, other_idx) =
                    binding.expect("non-unit case must have payload bindings");
                let self_name = format!("__eq_self_{case_name}_{self_idx}");
                let other_name = format!("__eq_other_{case_name}_{other_idx}");
                let self_payload = local_expr(self_idx, &self_name, *payload_type, span);
                let other_payload = local_expr(other_idx, &other_name, *payload_type, span);
                let eq_result = eq_call_expr(
                    self_payload,
                    other_payload,
                    *payload_type,
                    trait_env,
                    module_source,
                    tt,
                    span,
                );
                let matched_body = TirExpr::new(
                    TirExprKind::Block(TirBlock::new(
                        vec![TirStmt::new(
                            TirStmtKind::Return {
                                value: Some(eq_result),
                            },
                            span,
                        )],
                        span,
                    )),
                    TypeTable::UNIT,
                    span,
                );
                let inner_arms = vec![
                    TirMatchArm {
                        pattern: TirPattern::Variant {
                            enum_type: variant_type,
                            variant_name: case_name.clone(),
                            bindings: vec![TirPattern::Binding {
                                name: other_name,
                                local_index: other_idx,
                                type_id: *payload_type,
                            }],
                            payload_type: *payload_type,
                        },
                        guard: None,
                        body: matched_body,
                        span,
                    },
                    TirMatchArm {
                        pattern: TirPattern::Wildcard,
                        guard: None,
                        body: return_bool(false),
                        span,
                    },
                ];
                let self_bindings = vec![TirPattern::Binding {
                    name: self_name,
                    local_index: self_idx,
                    type_id: *payload_type,
                }];
                (self_bindings, inner_arms)
            };
            let inner_match = TirExpr::new(
                TirExprKind::Match {
                    expr: Box::new(deref_other()),
                    arms: inner_arms,
                },
                TypeTable::UNIT,
                span,
            );
            TirMatchArm {
                pattern: TirPattern::Variant {
                    enum_type: variant_type,
                    variant_name: case_name.clone(),
                    bindings: self_bindings,
                    payload_type: *payload_type,
                },
                guard: None,
                body: inner_match,
                span,
            }
        })
        .collect();

    let outer_match = TirExpr::new(
        TirExprKind::Match {
            expr: Box::new(deref_self),
            arms: outer_arms,
        },
        TypeTable::UNIT,
        span,
    );
    vec![TirStmt::new(TirStmtKind::Expr(outer_match), span)]
}
