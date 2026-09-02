//! Serde synthesis phase.
//!
//! Generates struct `Deserialize`, and the `FieldSchema` lookup it reads, for
//! types with a synthesis request. Every other body is derived in Wado, by the
//! `Reflect*` blankets in `core:serde` (WEP 2026-06-13); this pass still drains
//! their requests, which is what makes the bound demand the reflection impls.

use std::cell::RefCell;
use std::rc::Rc;

use crate::compiler_item::CompilerItem;
use crate::hashmap::{IndexMap, IndexSet};

use crate::module_source::ModuleSource;
use crate::name::{FqTypeName, LocalMethodName, MethodName, mangle_local_trait_method};
use crate::package::Package;
use crate::tir::{
    CallArg, FunctionKind, FunctionRef, InlineHint, ResolvedType, SynthTrait, SynthesisRequest,
    TirBinaryOp, TirExpr, TirExprKind, TirFunction, TirLocal, TirModule, TirParam, TirStmt, TypeId,
    TypeTable,
};
use crate::token::Span;

/// Snapshot of the `core:serde` symbol names this synthesiser needs, resolved
/// once through the `CompilerItem` registry. Mirrors
/// [`super::cm_binding::types::CmStdlibNames`]: built per-call, cheap (a
/// registry hit plus a clone), and threaded through the synthesis helpers so a
/// stdlib rename flows through every call site without touching Rust code.
///
/// Field set is **minimal**: with the deserialize body derived in Wado, the
/// only symbol left is the trait the synthesized `lookup` / `positional_at`
/// implement.
#[derive(Clone, Debug)]
pub(super) struct SerdeStdlibNames {
    pub deserialize: crate::name::FqTraitName,
    pub field_schema: crate::name::FqTraitName,
}

impl SerdeStdlibNames {
    pub fn from_type_table(type_table: &crate::tir::TypeTable) -> Self {
        let items = type_table.compiler_items();
        Self {
            deserialize: items.trait_fq(CompilerItem::Deserialize),
            field_schema: items.trait_fq(CompilerItem::FieldSchema),
        }
    }
}

use super::common::{
    alloc_local, alloc_named_local, block, i32_const, if_stmt, let_stmt, local_ref, option_none,
    option_some, param_local, return_stmt, synth_span,
};

/// Wire-form name for a struct field.
///
/// Single source of truth for the serialized key: an explicit
/// `#[wire(name = "...")]` wins; otherwise `#[wire(name_policy = "...")]`
/// applies its case strategy; otherwise the Wado source field name is used
/// verbatim (identity — `user_id` stays `"user_id"`, `userId` stays
/// `"userId"`). Used by both the serialize and deserialize synthesisers so the
/// two never disagree.
fn serialized_field_name(f: &crate::tir::TirField, struct_def: &crate::tir::TirStruct) -> String {
    f.wire_name_override.clone().unwrap_or_else(|| {
        if let Some(strategy) = &struct_def.wire_name_policy {
            apply_name_policy(&f.name, strategy)
        } else {
            f.name.clone()
        }
    })
}

/// Apply a `name_policy` strategy. Source-casing-agnostic, so it works for both
/// `snake_case` struct fields and `PascalCase` enum/variant cases (Wado casing is
/// convention, not a rule, so the source form is open).
fn apply_name_policy(s: &str, strategy: &str) -> String {
    use heck::{
        ToKebabCase, ToLowerCamelCase, ToShoutyKebabCase, ToShoutySnakeCase, ToSnakeCase,
        ToUpperCamelCase,
    };
    match strategy {
        "camelCase" => s.to_lower_camel_case(),
        "snake_case" => s.to_snake_case(),
        "PascalCase" => s.to_upper_camel_case(),
        "SCREAMING_SNAKE_CASE" => s.to_shouty_snake_case(),
        "kebab-case" => s.to_kebab_case(),
        "SCREAMING-KEBAB-CASE" => s.to_shouty_kebab_case(),
        // Unrecognized strategy string: fall back to identity (the name as
        // written), matching the no-attribute default.
        _ => s.to_string(),
    }
}

pub fn synthesize_serde(project: &mut Package) {
    distribute_bound_driven_requests(project);

    for module in project.tir_modules.values_mut() {
        let requests = std::mem::take(&mut module.synthesis_requests);
        if requests.is_empty() {
            continue;
        }
        let names = {
            let tt = module.type_table.borrow();
            SerdeStdlibNames::from_type_table(&tt)
        };
        let existing = collect_existing_trait_methods(module);
        let mut generated = Vec::new();

        for req in &requests {
            match req.trait_ref {
                // `Serialize` is derived in Wado, by the blanket impls over
                // `ReflectStruct` / `ReflectVariant` / `ReflectEnum` /
                // `ReflectFlags` in `core:serde`. The request still arrives —
                // it is what makes the bound demand the reflection impls — and
                // there is nothing here left to generate for it.
                SynthTrait::Serialize => {}
                SynthTrait::Deserialize => {
                    let key = MethodName::format_local(
                        &module
                            .type_table
                            .borrow()
                            .fq_base_type_name(req.target_type_id),
                        Some(&names.deserialize),
                        "deserialize",
                    );
                    if existing.contains(&key) {
                        continue;
                    }
                    if let Some((lookup_func, positional_at_func)) =
                        generate_field_schema(module, req, &names)
                    {
                        generated.push(Rc::new(RefCell::new(lookup_func)));
                        generated.push(Rc::new(RefCell::new(positional_at_func)));
                    }
                }
                // `From` requests are drained by `from_synth`, which runs
                // before this pass, so none reach the serde drain.
                SynthTrait::From { .. } => unreachable!(
                    "From synthesis requests must be drained by from_synth before serde_synth"
                ),
            }
        }

        module.functions.extend(generated);
    }
}

/// Distribute bound-driven `Serialize` / `Deserialize` requests (WEP
/// 2026-06-25) into each request's own defining module — where an explicit
/// `impl Trait for T;` marker's request already lives — so the drain loop above
/// sees one uniform list. Reads `bound_driven_synth_requests` as a snapshot, not
/// a drain: `synthesize_traits` reads the same set for its own entries.
fn distribute_bound_driven_requests(project: &mut Package) {
    let Some(type_table) = project
        .tir_modules
        .values()
        .next()
        .map(|m| m.type_table.clone())
    else {
        return;
    };

    let (serialize_key, deserialize_key) = {
        let tt = type_table.borrow();
        let items = tt.compiler_items();
        (
            items
                .trait_fq_opt(CompilerItem::Serialize)
                .and_then(|t| t.canonical()),
            items
                .trait_fq_opt(CompilerItem::Deserialize)
                .and_then(|t| t.canonical()),
        )
    };

    // Fetch and filter to just ours (Eq/Ord entries belong to
    // `synthesize_traits`) before the scan below, so a program with no
    // bound-driven serde requests skips it entirely.
    let requests = type_table.borrow().bound_driven_synth_requests(|key| {
        Some(key) == serialize_key.as_ref() || Some(key) == deserialize_key.as_ref()
    });
    if requests.is_empty() {
        return;
    }

    // One pass to resolve every declared struct/enum/variant/flags head to
    // its `TypeId` (`SynthesisRequest` needs one), instead of an O(requests
    // × types) rescan per entry. Keyed by the head the request recorded, so
    // two modules' same-named declarations stay apart.
    let by_head: IndexMap<crate::name::TypeHead, TypeId> = {
        let tt = type_table.borrow();
        tt.all_types()
            .filter_map(|(id, resolved)| match resolved {
                ResolvedType::Struct { .. }
                | ResolvedType::Enum { .. }
                | ResolvedType::Variant { .. }
                | ResolvedType::Flags { .. } => Some((tt.fq_base_type_name(id).head().clone(), id)),
                _ => None,
            })
            .collect()
    };

    for (target_head, module_source, trait_key) in requests {
        let trait_ref = if Some(&trait_key) == serialize_key.as_ref() {
            SynthTrait::Serialize
        } else {
            assert_eq!(
                Some(&trait_key),
                deserialize_key.as_ref(),
                "`requests` is filtered to the two serde traits"
            );
            SynthTrait::Deserialize
        };
        let Some(&target_type_id) = by_head.get(&target_head) else {
            continue;
        };
        let Some(module) = project.tir_modules.get_mut(&module_source) else {
            continue;
        };
        module.synthesis_requests.push(SynthesisRequest {
            trait_ref,
            target_type_name: target_head.rendered().to_string(),
            target_type_id,
            type_params: Vec::new(),
            span: Span::default(),
        });
    }
}

fn collect_existing_trait_methods(module: &TirModule) -> IndexSet<String> {
    module
        .functions
        .iter()
        .filter_map(|f| {
            let func = f.borrow();
            func.method_info.as_ref().and_then(|info| {
                info.trait_name.as_ref().map(|trait_name| {
                    mangle_local_trait_method(
                        &info.base_struct_name(),
                        &trait_name.to_mangled(),
                        &info.method_name,
                    )
                })
            })
        })
        .collect()
}

fn find_struct<'a>(module: &'a TirModule, name: &str) -> Option<&'a crate::tir::TirStruct> {
    module.structs.iter().find(|s| s.name == name)
}

/// Generate the `FieldSchema` impl a struct's `Deserialize` derivation reads:
/// `lookup` maps a wire key's bytes to a field index, `positional_at` maps an
/// ordinal rank to one. The deserialize body itself is derived in Wado, by the
/// `ReflectStruct` blanket in `core:serde` (WEP 2026-06-13).
fn generate_field_schema(
    module: &TirModule,
    req: &crate::tir::SynthesisRequest,
    names: &SerdeStdlibNames,
) -> Option<(TirFunction, TirFunction)> {
    let struct_def = find_struct(module, &req.target_type_name)?;
    let span = synth_span();

    let mut tt = module.type_table.borrow_mut();
    let option_i32 = tt.make_option(TypeTable::I32);
    // `FieldSchema::lookup(key: ByteSlice)`: the slice module comes from the
    // byte-read compiler item, wrapped in the `ByteSlice` newtype to match the
    // trait signature.
    let key_slice_type = {
        let base = {
            let def = tt.require_compiler_item_def(crate::compiler_item::CompilerItem::Slice);
            tt.make_generic_instance(def, vec![TypeTable::U8])
        };
        let def = tt.require_compiler_item_def(crate::compiler_item::CompilerItem::ByteSlice);
        tt.make_newtype(def, base)
    };
    let fields: Vec<(String, String, TypeId, u32)> = struct_def
        .fields
        .iter()
        .map(|f| {
            let serialized_name = serialized_field_name(f, struct_def);
            (f.name.clone(), serialized_name, f.type_id, f.index)
        })
        .collect();
    // `next_field::<Type>()` resolves `FieldSchema::lookup` by substituting the
    // concrete receiver, so the definition must carry the same fq receiver the
    // substitution builds.
    let target_fq = tt.fq_base_type_name(req.target_type_id);
    let compiler_items = tt.compiler_items().clone();
    drop(tt);

    // `#[wire(positional)]` flags, aligned with `fields` (enumerate index ==
    // the field index the derivation writes). A positional field is ordinal:
    // `lookup` omits it, `positional_at` enumerates it.
    let positional_flags: Vec<bool> = struct_def
        .fields
        .iter()
        .map(|f| f.serde_positional)
        .collect();

    let mut lookup_func = generate_lookup_function(
        &target_fq,
        &names.field_schema,
        &fields,
        &positional_flags,
        key_slice_type,
        option_i32,
        span,
        &compiler_items,
    );
    let mut positional_at_func = generate_positional_at_function(
        &target_fq,
        &names.field_schema,
        &positional_flags,
        option_i32,
        span,
        &compiler_items,
    );
    // A generic struct's schema is one impl over `S<T, …>`, like its reflect
    // impls: the derivation calls `next_field::<T>()` with the instance, so the
    // methods must instantiate alongside it. Neither body reads the parameters.
    lookup_func
        .impl_type_params
        .clone_from(&struct_def.type_params);
    positional_at_func
        .impl_type_params
        .clone_from(&struct_def.type_params);
    Some((lookup_func, positional_at_func))
}

/// Build a `key.get_unchecked(index_expr) as i32` expression on a
/// `ByteSlice` (`Slice<u8>`) key, with a computed index.
///
/// The method is looked up via [`CompilerItem::ByteSliceGetUnchecked`] —
/// renaming the underlying Wado declaration cannot break this site as
/// long as its `#[compiler_item("byte_slice_get_unchecked")]` annotation
/// stays put. See issue #1077 for the rationale.
fn key_get_byte_as_i32_expr(
    key_ref: TirExpr,
    index_expr: TirExpr,
    span: Span,
    compiler_items: &crate::compiler_item::CompilerItems,
) -> TirExpr {
    let get_byte_call = byte_slice_method_call(
        crate::compiler_item::CompilerItem::ByteSliceGetUnchecked,
        key_ref,
        vec![CallArg::new(index_expr, false)],
        TypeTable::U8,
        span,
        compiler_items,
    );
    TirExpr::new(
        TirExprKind::Cast {
            expr: Box::new(get_byte_call),
            target_type: TypeTable::I32,
        },
        TypeTable::I32,
        span,
    )
}

/// Build a `key.len()` expression on a `ByteSlice` (`Slice<u8>`) key,
/// returning the byte length as `i32`.
fn byte_slice_len_expr(
    key_ref: TirExpr,
    span: Span,
    compiler_items: &crate::compiler_item::CompilerItems,
) -> TirExpr {
    byte_slice_method_call(
        crate::compiler_item::CompilerItem::ByteSliceLen,
        key_ref,
        vec![],
        TypeTable::I32,
        span,
        compiler_items,
    )
}

/// Build a call to a generic `Slice<T>` method on a `ByteSlice`
/// (`Slice<u8>`) receiver, monomorphized at `T = u8`.
///
/// The method is resolved via `#[compiler_item]` (rename-safe, per issue
/// #1077) and called with `impl_type_args = [u8]` so the monomorphizer
/// instantiates the `u8` specialization instead of inheriting the enclosing
/// function's type parameters — the same shape as the generic `default()`
/// call synthesised for field defaults.
fn byte_slice_method_call(
    item: crate::compiler_item::CompilerItem,
    receiver: TirExpr,
    args: Vec<CallArg>,
    return_type: TypeId,
    span: Span,
    compiler_items: &crate::compiler_item::CompilerItems,
) -> TirExpr {
    let (module_source, _, name) = compiler_items.require_method(item);
    let module_source = module_source.clone();
    let method_info = LocalMethodName::new(
        compiler_items.require_method_owner(item).clone(),
        None,
        name.to_string(),
    )
    .with_struct_type_args(&[FqTypeName::builtin("u8")]);
    let monomorph_info = crate::tir::MonomorphInfo {
        generic_name: method_info.base_struct_name(),
        impl_type_args: vec![TypeTable::U8],
        method_type_args: vec![],
        is_blanket: false,
    };
    TirExpr::new(
        TirExprKind::method_call(
            Box::new(receiver),
            FunctionRef {
                module_source,
                name: method_info.to_mangled_name(),
                monomorph_info: Some(monomorph_info),
                method_info: Some(method_info),
            },
            vec![],
            args,
        ),
        return_type,
        span,
    )
}

/// Build `left && right` expression.
fn and_expr(left: TirExpr, right: TirExpr, span: Span) -> TirExpr {
    TirExpr::new(
        TirExprKind::Binary {
            left: Box::new(left),
            op: TirBinaryOp::And,
            right: Box::new(right),
        },
        TypeTable::BOOL,
        span,
    )
}

/// Build `left == right` expression for i32 operands.
fn i32_eq(left: TirExpr, right: TirExpr, span: Span) -> TirExpr {
    TirExpr::new(
        TirExprKind::Binary {
            left: Box::new(left),
            op: TirBinaryOp::Eq,
            right: Box::new(right),
        },
        TypeTable::BOOL,
        span,
    )
}

/// Assemble a static `FieldSchema` method (`lookup` / `positional_at`): a
/// no-`self`, single-`i32`-or-bytes-param function returning `Option<i32>`.
/// Both methods share this boilerplate; only the name, parameter, and body
/// differ.
#[allow(clippy::too_many_arguments)]
fn field_schema_method_fn(
    type_name: &FqTypeName,
    field_schema_trait: &crate::name::FqTraitName,
    method: &str,
    param_name: &str,
    param_type: TypeId,
    return_type: TypeId,
    locals: Vec<TirLocal>,
    local_count: u32,
    body: Vec<TirStmt>,
    span: Span,
) -> TirFunction {
    TirFunction {
        module_source: ModuleSource::default(),
        name: MethodName::format_local(type_name, Some(field_schema_trait), method),
        def_id: None,
        visibility: crate::ast::Visibility::Public,
        is_export: false,
        is_cm_export: false,
        is_ambient: false,
        benign_effects: Vec::new(),
        is_async: false,
        type_params: Vec::new(),
        impl_type_params: Vec::new(),
        monomorph_info: None,
        method_info: Some(LocalMethodName::new(
            type_name.clone(),
            Some(field_schema_trait.clone()),
            method.to_string(),
        )),
        params: vec![TirParam {
            name: param_name.to_string(),
            type_id: param_type,
            local_index: 0,
            is_mut: false,
            is_mut_ref: false,
            span,
        }],
        return_type,
        task_return_type: None,
        effects: Vec::new(),
        stores: vec![],
        body: Some(block(body)),
        span,
        local_count,
        locals,
        address_taken_locals: IndexSet::default(),
        stores_aliased_locals: IndexSet::default(),
        is_cm_binding: false,
        is_dispatch_wrapper: false,
        inline_hint: InlineHint::Auto,
        compiler_item: None,
        export_name: None,
        allocator_tag: None,
        kind: FunctionKind::Regular,
        return_abi: crate::tir::ReturnAbi::default(),
    }
}

/// A wire key and the field index it selects.
#[derive(Clone, Copy)]
struct Key<'a> {
    bytes: &'a [u8],
    index: i32,
}

/// Emits `lookup` as a decision tree over the key's bytes, so a key costs one
/// walk down the tree rather than one test per declared field.
struct LookupTree<'a> {
    key_slice_type: TypeId,
    option_i32: TypeId,
    span: Span,
    compiler_items: &'a crate::compiler_item::CompilerItems,
    locals: Vec<TirLocal>,
    next_local: u32,
}

impl LookupTree<'_> {
    fn byte_at(&self, pos: usize) -> TirExpr {
        key_get_byte_as_i32_expr(
            local_ref(0, "__key", self.key_slice_type),
            i32_const(pos as i32),
            self.span,
            self.compiler_items,
        )
    }

    fn found(&self, index: i32) -> TirStmt {
        return_stmt(Some(option_some(
            i32_const(index),
            self.option_i32,
            self.compiler_items,
        )))
    }

    fn not_found(&self) -> TirStmt {
        return_stmt(Some(option_none(self.option_i32, self.compiler_items)))
    }

    /// Confirm the positions the walk has not already decided, then report the
    /// field. Falls through when one of them differs.
    fn confirm(&self, key: &Key, decided: &[usize]) -> TirStmt {
        let found = self.found(key.index);
        match self.byte_tests(key, undecided(key.bytes.len(), decided)) {
            Some(condition) => if_stmt(condition, block(vec![found]), None),
            None => found,
        }
    }

    /// The condition testing that `positions` hold `key`'s own bytes. `None`
    /// when `positions` is empty.
    fn byte_tests(&self, key: &Key, positions: impl Iterator<Item = usize>) -> Option<TirExpr> {
        positions
            .map(|pos| {
                i32_eq(
                    self.byte_at(pos),
                    i32_const(i32::from(key.bytes[pos])),
                    self.span,
                )
            })
            .reduce(|acc, eq| and_expr(acc, eq, self.span))
    }

    /// Statements that return whichever of `keys` the wire key names, or fall
    /// through. `decided` holds the positions the walk has pinned to one byte.
    fn dispatch(&mut self, keys: &[Key], decided: &mut Vec<usize>) -> Vec<TirStmt> {
        if let [only] = keys {
            return vec![self.confirm(only, decided)];
        }
        assert!(
            keys.iter().all(|k| k.bytes.len() == keys[0].bytes.len()),
            "a subtree stays inside one length bucket: skipping a decided \
             position is sound only because the length branch above pinned it \
             for every key still in play"
        );
        // A position the whole group agrees on costs one test for the group. A
        // leaf would pay it once per key.
        let shared: Vec<usize> = undecided(keys[0].bytes.len(), decided)
            .filter(|&p| keys.iter().all(|k| k.bytes[p] == keys[0].bytes[p]))
            .collect();
        let Some(condition) = self.byte_tests(&keys[0], shared.iter().copied()) else {
            return self.split(keys, decided);
        };
        decided.extend(&shared);
        let body = self.split(keys, decided);
        decided.truncate(decided.len() - shared.len());
        vec![if_stmt(condition, block(body), None)]
    }

    /// Branch `keys` on the byte that tells them apart best.
    fn split(&mut self, keys: &[Key], decided: &mut Vec<usize>) -> Vec<TirStmt> {
        let Some(pos) = split_position(keys, decided) else {
            // Two fields share a wire name, so nothing tells them apart. The
            // first declared wins.
            return keys.iter().map(|k| self.confirm(k, decided)).collect();
        };
        let groups = group_by_byte(keys, pos);
        let name = format!("__b{pos}");
        let local = alloc_named_local(
            &mut self.next_local,
            &mut self.locals,
            Some(name.clone()),
            TypeTable::I32,
            false,
        );
        // Bound to a local rather than read inside each guard: `if_chain_to_match`
        // fuses a run of `if K == <local>` into one `Match`, which lowers to a
        // switch.
        let mut stmts = vec![let_stmt(&name, local, TypeTable::I32, self.byte_at(pos))];
        decided.push(pos);
        for (byte, group) in groups {
            let condition = i32_eq(
                local_ref(local, &name, TypeTable::I32),
                i32_const(i32::from(byte)),
                self.span,
            );
            let mut body = self.dispatch(&group, decided);
            body.push(self.not_found());
            stmts.push(if_stmt(condition, block(body), None));
        }
        decided.pop();
        stmts
    }
}

/// The positions of a `len`-byte key the walk has not pinned to one byte yet.
fn undecided(len: usize, decided: &[usize]) -> impl Iterator<Item = usize> + '_ {
    (0..len).filter(|p| !decided.contains(p))
}

/// The undecided position splitting `keys` into the most groups; ties go to the
/// lowest. `None` when no position splits them, which needs two equal keys.
fn split_position(keys: &[Key], decided: &[usize]) -> Option<usize> {
    undecided(keys[0].bytes.len(), decided)
        .map(|p| (group_by_byte(keys, p).len(), p))
        .filter(|&(groups, _)| groups > 1)
        .max_by_key(|&(groups, p)| (groups, std::cmp::Reverse(p)))
        .map(|(_, p)| p)
}

/// `keys` bucketed by their byte at `pos`. Buckets and contents both keep
/// declaration order.
fn group_by_byte<'a>(keys: &[Key<'a>], pos: usize) -> IndexMap<u8, Vec<Key<'a>>> {
    let mut groups: IndexMap<u8, Vec<Key<'a>>> = IndexMap::default();
    for key in keys {
        groups.entry(key.bytes[pos]).or_default().push(*key);
    }
    groups
}

fn generate_lookup_function(
    type_name: &FqTypeName,
    field_schema_trait: &crate::name::FqTraitName,
    fields: &[(String, String, TypeId, u32)],
    positional_flags: &[bool],
    key_slice_type: TypeId,
    option_i32: TypeId,
    span: Span,
    compiler_items: &crate::compiler_item::CompilerItems,
) -> TirFunction {
    let mut tree = LookupTree {
        key_slice_type,
        option_i32,
        span,
        compiler_items,
        locals: vec![param_local("__key", key_slice_type, false)],
        next_local: 1,
    };

    let len_local = alloc_local(&mut tree.next_local, &mut tree.locals, TypeTable::I32);
    let len_expr = byte_slice_len_expr(local_ref(0, "__key", key_slice_type), span, compiler_items);
    let mut stmts = vec![let_stmt("__len", len_local, TypeTable::I32, len_expr)];

    // A positional field is ordinal, never matched by name, so the tree omits
    // it. `positional_at` enumerates it instead.
    let mut by_len: IndexMap<usize, Vec<Key>> = IndexMap::default();
    for (index, (_, wire_name, _, _)) in fields.iter().enumerate() {
        if positional_flags.get(index).copied().unwrap_or(false) {
            continue;
        }
        let key = Key {
            bytes: wire_name.as_bytes(),
            index: index as i32,
        };
        by_len.entry(key.bytes.len()).or_default().push(key);
    }
    // Length roots the tree. It discriminates without reading a byte, and every
    // byte the branches below it read is in range only because it ran first.
    for (len, group) in by_len {
        let condition = i32_eq(
            local_ref(len_local, "__len", TypeTable::I32),
            i32_const(len as i32),
            span,
        );
        let mut body = tree.dispatch(&group, &mut Vec::new());
        body.push(tree.not_found());
        stmts.push(if_stmt(condition, block(body), None));
    }
    stmts.push(tree.not_found());

    field_schema_method_fn(
        type_name,
        field_schema_trait,
        "lookup",
        "__key",
        key_slice_type,
        option_i32,
        tree.locals,
        tree.next_local,
        stmts,
        span,
    )
}

/// `impl FieldSchema for <Type> { fn positional_at(rank: i32) -> Option<i32> }`
/// — the static, per-type ordinal-field matcher. Maps the `rank`-th
/// `#[wire(positional)]` field (in declaration order) to its field index;
/// returns `null` for an out-of-range rank, and for every rank when the type
/// has no positional fields. `positional_flags` is aligned with the deserialize
/// loop's field indices, so the returned index drives the same `field == i`
/// assignment as `lookup`.
fn generate_positional_at_function(
    type_name: &FqTypeName,
    field_schema_trait: &crate::name::FqTraitName,
    positional_flags: &[bool],
    option_i32: TypeId,
    span: Span,
    compiler_items: &crate::compiler_item::CompilerItems,
) -> TirFunction {
    let locals = vec![param_local("__rank", TypeTable::I32, false)];
    let next_local: u32 = 1;

    let mut stmts = Vec::new();

    // For each positional field (in declaration order), generate:
    //   if __rank == R { return Some(field_index); }
    let mut rank: i32 = 0;
    for (field_index, &is_positional) in positional_flags.iter().enumerate() {
        if !is_positional {
            continue;
        }
        let condition = i32_eq(
            local_ref(0, "__rank", TypeTable::I32),
            i32_const(rank),
            span,
        );
        stmts.push(if_stmt(
            condition,
            block(vec![return_stmt(Some(option_some(
                i32_const(field_index as i32),
                option_i32,
                compiler_items,
            )))]),
            None,
        ));
        rank += 1;
    }
    stmts.push(return_stmt(Some(option_none(option_i32, compiler_items))));

    field_schema_method_fn(
        type_name,
        field_schema_trait,
        "positional_at",
        "__rank",
        TypeTable::I32,
        option_i32,
        locals,
        next_local,
        stmts,
        span,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ground-truth heck outputs for an edge corpus. `core:serde::apply_case`
    /// (the library port used by reflect-based derivations) must match these
    /// exact strings — the same vectors are asserted in `serde_test.wado`. If
    /// heck changes, this fails first and the Wado vectors are updated in step.
    #[test]
    fn rename_all_edge_corpus_locks_heck_output() {
        let snake = |s: &str| apply_name_policy(s, "snake_case");
        assert_eq!(snake("userID"), "user_id");
        assert_eq!(snake("HTTPStatus"), "http_status");
        assert_eq!(snake("parseHTTP"), "parse_http");
        assert_eq!(snake("IOError"), "io_error");
        assert_eq!(snake("iOS"), "i_os");
        assert_eq!(snake("field2Name"), "field2_name");
        assert_eq!(snake("Apple2Banana"), "apple2_banana");
        assert_eq!(apply_name_policy("HTTPStatus", "camelCase"), "httpStatus");
        assert_eq!(apply_name_policy("userID", "PascalCase"), "UserId");
    }

    #[test]
    fn rename_all_from_snake_field() {
        assert_eq!(apply_name_policy("user_name", "camelCase"), "userName");
        assert_eq!(apply_name_policy("user_name", "snake_case"), "user_name");
        assert_eq!(apply_name_policy("user_name", "PascalCase"), "UserName");
        assert_eq!(apply_name_policy("user_name", "kebab-case"), "user-name");
        assert_eq!(
            apply_name_policy("user_name", "SCREAMING_SNAKE_CASE"),
            "USER_NAME"
        );
    }

    #[test]
    fn rename_all_from_pascal_case() {
        // PascalCase source, not the snake_case of struct fields.
        assert_eq!(apply_name_policy("AddRemote", "kebab-case"), "add-remote");
        assert_eq!(apply_name_policy("AddRemote", "snake_case"), "add_remote");
        assert_eq!(apply_name_policy("AddRemote", "camelCase"), "addRemote");
        assert_eq!(apply_name_policy("List", "kebab-case"), "list");
    }
}
