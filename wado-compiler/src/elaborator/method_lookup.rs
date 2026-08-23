//! Method lookup, operator resolution, and indexing trait dispatch.

use super::trait_env::ImplTargetKey;
use std::rc::Rc;
use std::sync::Arc;

use crate::hashmap::{IndexMap, IndexSet};

use crate::ast::{self, AstId, BinaryOp, Expr, Type};
use crate::compiler_host::CompilerHost;
use crate::compiler_item::CompilerItem;
use crate::module_source::ModuleSource;
use crate::name::{LocalMethodName, MethodName, RefKind};
use crate::tir::{
    CallArg, FunctionRef, ResolvedType, TirExpr, TirExprKind, TirUnaryOp, TypeId, TypeTable,
};
use crate::token::Span;

use super::Elaborator;
use super::infer::InferCtx;
use super::instantiate::Instantiation;
use super::sig::InstantiatedImplSig;
use super::synth::{ArgClass, ArgProbe};
use super::trait_env::{ImplHeader, TraitEnv};
use super::types::{
    ArithmeticTraitInfo, FunctionContext, IndexAssignTraitInfo, IndexMutTraitInfo, IndexTraitInfo,
    IndexValueTraitInfo, KeyValueLiteralTraitInfo, MethodInfo, MethodOwner,
    SequenceLiteralTraitInfo, TypeError, TypeLookup,
};
use super::tysys::TypeSystem;

use super::util::placeholder;

/// Shared so the explicit `&mut x.f` and the implicit `&mut self` borrow say
/// the same thing about the same refusal.
pub(super) const REPLACE_ON_ASSIGN_PLACE: &str = "a field or element of a replace-on-assign type (primitive, enum, flags, fn); \
     use the containing value's reference directly";

/// Lightweight reference to an impl block. Stores `(module_source,
/// item_id)` and resolves to the block's digested [`ImplHeader`] via
/// [`impl_header`]. Dispatch cannot reach the impl AST at all.
struct ImplBlockRef(ModuleSource, AstId);

/// The digested header of the impl block `r` points at. Borrowed from the
/// caller's `TraitEnv` handle rather than from `&self`, so the header stays
/// readable across the `&mut self` calls a lookup makes.
///
/// Every `ImplBlockRef` originates from a `TraitEnv` impl index, and
/// `TraitEnv::build` writes `impl_headers` from the same walk that fills
/// those indices, so a miss means the two diverged.
fn impl_header<'a>(trait_env: &'a TraitEnv, r: &ImplBlockRef) -> &'a ImplHeader {
    trait_env
        .impl_headers
        .get(&(r.0.clone(), r.1))
        .expect("every indexed impl block has an ImplHeader")
}

impl<H: CompilerHost> Elaborator<'_, H> {
    /// The declaration facts the decl pass recorded for an indexed impl block.
    fn impl_sig(&self, r: &ImplBlockRef) -> &super::sig::ImplSig {
        self.tysys
            .signatures
            .impl_sig(r.1)
            .expect("the decl pass records every impl block's declaration facts")
    }
}

/// Inputs for [`Elaborator::infer_method_type_args`].
///
/// Groups what the lookup already resolved about the method — its slots, in
/// both their signature and declaration forms, and its parameter and return
/// types — with the call-site context the solver needs.
pub(super) struct MethodInferenceInput<'a> {
    /// Receiver's `TypeId` at the call site (any reference level; the
    /// helper strips references internally).
    pub receiver_type: TypeId,
    /// Method name, for the "cannot infer" diagnostic.
    pub method_name: &'a str,
    /// The method's own slots, as the lookup that produced `param_types`
    /// reported them ([`MethodInfo::method_type_param_ids`]). The answers
    /// come back in this order, which is the order the caller binds them in.
    pub slots: &'a [TypeId],
    /// The same slots as the declaration wrote them
    /// ([`MethodInfo::method_own_params`]), parallel to `slots`.
    pub own_params: &'a [ast::GenericParam],
    /// Method parameter types in their `TypeParam`-based (uninstantiated)
    /// form; parallel to `args` / `raw_args`.
    pub param_types: &'a [TypeId],
    /// Already-resolved argument expressions, in order.
    pub args: &'a [TirExpr],
    /// Raw AST expressions for `args`, used to detect literal-number
    /// arguments that participate in deferred-literal unification.
    pub raw_args: &'a [Expr],
    /// Method declared return type in its `TypeParam`-based form; used for
    /// back-inference from `expected_return_type`.
    pub decl_return_type: TypeId,
    /// Expected return type at the call site (from a type annotation or
    /// surrounding call), used for back-inference.
    pub expected_return_type: Option<TypeId>,
    /// The dispatch-resolved trait, when this is a trait method. Disambiguates
    /// the method lookup for same-named methods on different traits (e.g.
    /// `payload` on Serialize vs Deserialize). A declaration, not a spelling:
    /// two modules' same-named traits are two traits.
    pub trait_decl: Option<crate::defs::DefId>,
    /// Module declaring the method. See
    /// [`Elaborator::fill_defaulted_method_type_args`].
    pub declaring_module: Option<ModuleSource>,
    /// Call-site span, used to anchor a "cannot infer type parameter"
    /// diagnostic when inference leaves a method type parameter dangling.
    pub span: Span,
}

/// The positions an `impl` target writes, `None` for a target writing none.
/// Naming and matching both read it here, so neither pins a position the
/// other leaves free (WEP 2026-08-12).
pub(super) fn impl_target_args(impl_ty: &Type) -> Option<&[Type]> {
    let inner = match impl_ty {
        Type::Reference(i) | Type::MutReference(i) => i.as_ref(),
        other => other,
    };
    match inner {
        Type::Tuple(elems) => Some(elems),
        other => impl_target_head_args(other),
    }
}

/// The head's own argument list — `Cell<T>` and `ns::Cell<T>` alike.
/// [`impl_target_args`] minus the tuple target, which binds as a variadic
/// pack through its own path.
pub(super) fn impl_target_head_args(impl_ty: &Type) -> Option<&[Type]> {
    match impl_ty {
        Type::Generic(g) => Some(&g.args),
        Type::NamespacedGeneric(ns) => Some(&ns.args),
        _ => None,
    }
}

impl TypeSystem {
    /// For an inherent `impl` on a possibly-generic type, check that any
    /// concrete type arguments written in the impl header (e.g. the `u8` in
    /// `impl List<u8>`) match the receiver's actual type arguments. Type
    /// parameters (e.g. `T` in `impl List<T>`) match any argument. This is
    /// what keeps `impl List<u8>` from applying to a `List<i32>` receiver.
    ///
    /// Non-generic impls (e.g. `impl i32`) impose no constraint here; the
    /// struct-name match already pinned the receiver type.
    pub(crate) fn inherent_impl_type_args_match(
        &self,
        impl_ty: &Type,
        receiver_type_args: Option<&[TypeId]>,
    ) -> bool {
        let Some(written) = impl_target_args(impl_ty) else {
            return true;
        };
        // No receiver type args supplied (an existence/bounds check that did not
        // thread them) — nothing to constrain against, so don't reject.
        let Some(args) = receiver_type_args else {
            return true;
        };
        for (i, arg) in written.iter().enumerate() {
            let Some(&recv) = args.get(i) else {
                return false;
            };
            // `bind_target_param` reaches only a bare argument, so a nested
            // binder gets no slot and a receiver matching it would have
            // nothing to instantiate. Declining makes that a diagnostic.
            if self.nests_a_binder(arg) {
                return false;
            }
            if !self.arg_matches(arg, recv) {
                return false;
            }
        }
        true
    }

    /// Whether `recv` is what the header wrote at this position, the header's
    /// own type parameters standing for anything. Structural, never rendered
    /// (WEP 2026-08-12 §4); a binder is free only where it stands, so
    /// `impl<T> Slot<[i32, T]>` still wants a pair.
    fn arg_matches(&self, written: &Type, recv: TypeId) -> bool {
        use crate::tir::ResolvedType;
        let tt = self.type_table.borrow();
        let resolved = tt.get(recv).clone();
        drop(tt);
        match written {
            Type::Reference(inner) => match resolved {
                ResolvedType::Ref(target) => self.arg_matches(inner, target),
                _ => false,
            },
            Type::MutReference(inner) => match resolved {
                ResolvedType::MutRef(target) => self.arg_matches(inner, target),
                _ => false,
            },
            Type::Tuple(elems) if elems.is_empty() => matches!(resolved, ResolvedType::Unit),
            Type::Tuple(elems) => {
                let tt = self.type_table.borrow();
                let is_tuple = matches!(
                    tt.fq_base_type_name(recv).head(),
                    crate::name::TypeHead::Tuple
                );
                let recv_elems = tt.generic_type_args(recv).unwrap_or_default();
                drop(tt);
                is_tuple
                    && recv_elems.len() == elems.len()
                    && elems
                        .iter()
                        .zip(recv_elems)
                        .all(|(e, r)| self.arg_matches(e, r))
            }
            // The impl is registered under the written spelling and the call
            // site looks one up under the receiver's, so it applies exactly
            // where the two agree. Looser, and the call has no impl to name.
            Type::Function(_) => {
                let written_name =
                    super::trait_env::written_type_arg(written, &self.resolutions).to_mangled();
                let recv_name = self.type_table.borrow().mangle_type_arg_for_generic(recv);
                written_name == recv_name
            }
            // A pack, an `_`, a parse error: nothing written to match against.
            Type::TypePackSpread(..) | Type::Infer(_) | Type::Error(_) => true,
            // The arms below read the receiver through readers that see past a
            // reference, so reference-ness is settled here instead: a reference
            // receiver is reached only by an argument pinning nothing.
            _ if matches!(resolved, ResolvedType::Ref(_) | ResolvedType::MutRef(_)) => {
                !self.arg_pins(written)
            }
            // A written head: a binder matches anything, a declaration matches
            // its own, and the arguments recurse.
            _ => {
                let Some(def) = crate::resolve::head_site(written)
                    .and_then(|site| self.resolutions.declared_if_walked(site))
                else {
                    // A binder, or a name reaching nothing: no one type to
                    // require, so it accepts whatever the receiver supplies.
                    return true;
                };
                let tt = self.type_table.borrow();
                // `TypeHead` compares a declaration by `DefId` and an
                // undeclared shape by its rendering, which is all it has.
                // `nominal_def` answers `None` for `i32` and `()`.
                let written_head = crate::name::FqTypeName::of_head(self.resolutions.defs(), def);
                if *written_head.head() != *tt.fq_base_type_name(recv).head() {
                    return false;
                }
                let recv_args = tt.generic_type_args(recv).unwrap_or_default();
                drop(tt);
                let written_args = match written {
                    Type::Generic(g) => g.args.as_slice(),
                    Type::NamespacedGeneric(ns) => ns.args.as_slice(),
                    _ => &[],
                };
                // A head written bare (`impl Slot<Box>`) constrains the head
                // alone; the receiver's own arguments are not its business.
                written_args.is_empty()
                    || (written_args.len() == recv_args.len()
                        && written_args
                            .iter()
                            .zip(recv_args)
                            .all(|(w, r)| self.arg_matches(w, r)))
            }
        }
    }

    /// [`Self::arg_pins`] under the name the naming side asks it by — one
    /// predicate, so a position pinned for naming is pinned for matching.
    pub(crate) fn impl_arg_pins_a_position(&self, arg: &Type) -> bool {
        self.arg_pins(arg)
    }

    /// Whether a binder appears *inside* `arg` rather than as `arg` itself,
    /// the only position the header can bind. Asked of each head's reference
    /// site, so `ns::Tag` beside an `impl<Tag>` binder stays a declaration.
    fn nests_a_binder(&self, arg: &Type) -> bool {
        fn walk(this: &TypeSystem, ty: &Type, inside: bool) -> bool {
            let is_binder = crate::resolve::head_site(ty).is_some_and(|site| {
                matches!(
                    this.resolutions.walked(site),
                    Some(crate::resolve::Resolution::Binder(_))
                )
            });
            if inside && is_binder {
                return true;
            }
            let nested = |args: &[Type]| args.iter().any(|a| walk(this, a, true));
            match ty {
                Type::Reference(inner) | Type::MutReference(inner) => walk(this, inner, true),
                // `[..T]` is the variadic form, bound by its own path.
                Type::Tuple(elems)
                    if elems.iter().any(|e| matches!(e, Type::TypePackSpread(..))) =>
                {
                    false
                }
                Type::Tuple(elems) => nested(elems),
                Type::Function(ft) => nested(&ft.params) || walk(this, &ft.return_type, true),
                Type::Generic(g) => nested(&g.args),
                Type::NamespacedGeneric(ns) => nested(&ns.args),
                _ => false,
            }
        }
        walk(self, arg, false)
    }

    /// Whether every head inside `arg` names a declaration, so the argument
    /// stands for one type rather than for whatever the receiver supplies.
    /// The site decides: a mangle spells an unresolved head as `Builtin`.
    fn arg_pins(&self, arg: &Type) -> bool {
        let nested_pin = |args: &[Type]| args.iter().all(|a| self.arg_pins(a));
        match arg {
            // A reference pins what it refers to; the kind is structural.
            Type::Reference(inner) | Type::MutReference(inner) => self.arg_pins(inner),
            // `[]` is the unit type and pins by itself.
            Type::Tuple(elems) => nested_pin(elems),
            // A function type is spelled by its whole shape, so a head in the
            // parameter list names as much as the return type does.
            Type::Function(ft) => nested_pin(&ft.params) && self.arg_pins(&ft.return_type),
            Type::Generic(g) => self.head_is_declared(arg) && nested_pin(&g.args),
            Type::NamespacedGeneric(ns) => self.head_is_declared(arg) && nested_pin(&ns.args),
            Type::Named(_) => self.head_is_declared(arg),
            // A pack, an `_`, a parse error: nothing to name.
            Type::TypePackSpread(..) | Type::Infer(_) | Type::Error(_) => false,
        }
    }

    /// Whether this type's head reaches a declaration. A binder and a name that
    /// reaches nothing both answer `false` — neither is one type.
    fn head_is_declared(&self, ty: &Type) -> bool {
        crate::resolve::head_site(ty)
            .and_then(|site| self.resolutions.declared_if_walked(site))
            .is_some()
    }
}

impl<H: CompilerHost> Elaborator<'_, H> {
    /// Get the module source for an `ImplBlockRef`.
    fn impl_block_module_source(&self, r: &ImplBlockRef) -> ModuleSource {
        r.0.clone()
    }

    /// Collect trait impl block references for a given type name.
    /// Returns lightweight `ImplBlockRef` values instead of cloning impl block data.
    fn collect_trait_impl_refs(&self, type_key: &ImplTargetKey) -> Vec<ImplBlockRef> {
        let mut refs = Vec::new();
        if let Some(entries) = self.tysys.trait_env.impl_index.get(type_key) {
            for entry in entries {
                if self
                    .tysys
                    .trait_env
                    .impl_headers
                    .get(entry)
                    .is_some_and(|h| h.trait_name.is_some())
                {
                    refs.push(ImplBlockRef(entry.0.clone(), entry.1));
                }
            }
        }
        refs
    }

    /// Collect trait impl block references for multiple type names.
    fn collect_trait_impl_refs_multi(&self, type_keys: &[ImplTargetKey]) -> Vec<ImplBlockRef> {
        let mut refs = Vec::new();
        for key in type_keys {
            if let Some(entries) = self.tysys.trait_env.impl_index.get(key) {
                for entry in entries {
                    if self
                        .tysys
                        .trait_env
                        .impl_headers
                        .get(entry)
                        .is_some_and(|h| h.trait_name.is_some())
                    {
                        refs.push(ImplBlockRef(entry.0.clone(), entry.1));
                    }
                }
            }
        }
        refs
    }

    /// Shared scan-and-map prologue behind `find_indexing_trait_impl`,
    /// `find_assoc_type_in_trait_impl`, and `find_arithmetic_trait_impl`: walk
    /// the trait impls on `target` whose name satisfies `trait_matches`
    /// (prefix for indexing / assoc, exact for arithmetic), align each
    /// candidate's slots against `concrete_type_args`, and return the first
    /// non-`None` `project`. Per-candidate filtering and projection live in
    /// `project` (which also receives the impl's declared type params);
    /// returning `None` skips the candidate.
    fn probe_trait_impls<R>(
        &mut self,
        target: &ImplTargetKey,
        concrete_type_args: &[TypeId],
        trait_matches: impl Fn(&str, Option<crate::defs::DefId>) -> bool,
        mut project: impl FnMut(
            &mut Self,
            &ImplBlockRef,
            &InstantiatedImplSig,
            &IndexSet<String>,
        ) -> Option<R>,
    ) -> Option<R> {
        let trait_env = Arc::clone(&self.tysys.trait_env);
        let signatures = Rc::clone(&self.tysys.signatures);
        let impl_refs = self.collect_trait_impl_refs(target);
        for impl_ref in &impl_refs {
            let header = impl_header(&trait_env, impl_ref);
            let trait_name = self.get_type_name(header.trait_type.as_ref().unwrap());
            if !trait_matches(&trait_name, header.trait_ref) {
                continue;
            }
            let impl_sig = signatures
                .impl_sig(impl_ref.1)
                .expect("the decl pass records every impl block's declaration facts")
                .instantiate(&self.tysys.type_table, concrete_type_args);
            let declared = self
                .tysys
                .build_declared_type_params(&header.ty, &header.type_params);
            if let Some(result) = project(self, impl_ref, &impl_sig, &declared) {
                return Some(result);
            }
        }
        None
    }

    /// [`Self::probe_trait_impls`] as a fold: every candidate's projection,
    /// in candidate order, instead of the first. Selection that is
    /// unique-or-error needs to see them all before it may pick one.
    fn collect_trait_impls<R>(
        &mut self,
        target: &ImplTargetKey,
        concrete_type_args: &[TypeId],
        trait_matches: impl Fn(&str, Option<crate::defs::DefId>) -> bool,
        mut project: impl FnMut(
            &mut Self,
            &ImplBlockRef,
            &InstantiatedImplSig,
            &IndexSet<String>,
        ) -> Option<R>,
    ) -> Vec<R> {
        let mut found = Vec::new();
        self.probe_trait_impls::<()>(target, concrete_type_args, trait_matches, |s, r, sig, d| {
            if let Some(projected) = project(s, r, sig, d) {
                found.push(projected);
            }
            None
        });
        found
    }

    /// Find the rhs parameter type for an operator trait on a struct type.
    /// Used to determine what type a literal rhs should be coerced to. `rhs`
    /// is the right operand's class, which selects among several `Add<Rhs>`
    /// impls before either operand has been given a type.
    ///
    /// A literal admits every numeric width, so leaving several admitted impls
    /// to the dispatch below would let the literal's *default* type pick the
    /// winner, and adding an impl would silently retarget an existing call
    /// (WEP 2026-07-31). `span` is the right operand's — the thing to annotate.
    pub(super) fn find_operator_rhs_type(
        &mut self,
        self_type_id: TypeId,
        op: &BinaryOp,
        rhs: Option<&ArgClass>,
        span: Span,
    ) -> Option<TypeId> {
        let (item, method_name) = super::tysys::operator_trait_method(op)?;
        let trait_ = self.tysys.compiler_trait_def(item)?;
        // A type parameter has no impl block to read the rhs off; its bounds
        // say it, and `Shl::shl(&self, rhs: u32)` is why a literal needs to be
        // told. A bound cannot vary the declared rhs, so no selection arises.
        let param_name = match self.tysys.type_table.borrow().get(self_type_id) {
            ResolvedType::TypeParam { name, .. } => Some(name.clone()),
            _ => None,
        };
        if let Some(param_name) = param_name {
            return self.bound_declared_rhs_type(&param_name, trait_, method_name, self_type_id);
        }
        let struct_name = self.tysys.struct_name_for_type(self_type_id)?;
        let admitted =
            self.find_arithmetic_trait_impls(&struct_name, self_type_id, trait_, method_name, rhs);
        self.report_ambiguous_operator_rhs(&admitted, self_type_id, *op, span);
        let [trait_info] = admitted.as_slice() else {
            return None;
        };
        // Unwrap the &T reference wrapper if present (e.g., rhs: &Self → return Self)
        trait_info.rhs_type.map(|t| {
            let resolved = self.tysys.type_table.borrow().get(t).clone();
            match resolved {
                ResolvedType::Ref(inner) => inner,
                _ => t,
            }
        })
    }

    /// The declaration an operator dispatches through, as a compiler item
    /// names it.
    pub(super) fn operator_trait_decl(&self, op: &BinaryOp) -> Option<crate::defs::DefId> {
        self.tysys
            .compiler_trait_def(super::tysys::operator_compiler_item(op)?)
    }

    /// The right-hand type `trait_`'s declaration gives `method_name`, read off
    /// whichever bound names that trait — a hint for typing a literal, so it
    /// reports no ambiguity of its own; the dispatch already does.
    fn bound_declared_rhs_type(
        &mut self,
        param_name: &str,
        trait_: crate::defs::DefId,
        method_name: &str,
        self_type_id: TypeId,
    ) -> Option<TypeId> {
        let bounds = self
            .annotate_ctx
            .trait_ctx
            .type_param_bounds
            .get(param_name)?
            .clone();
        let declared = bounds.iter().find_map(|bound| {
            let decl = self.trait_decl_at(bound.id, &bound.name)?;
            if decl != trait_ {
                return None;
            }
            let sig = &self.trait_sig_of(&decl)?.method(method_name)?.sig;
            sig.decl.param_types.get(sig.first_value_param()).copied()
        })?;
        // The same slots the dispatch binds, so the hint and the call agree on
        // what a bare bound means.
        let slots = self.bare_bound_slots(trait_, self_type_id);
        let substituted = self
            .tysys
            .type_table
            .borrow_mut()
            .substitute_type_params(declared, &slots);
        Some(self.tysys.type_table.borrow().peel_refs(substituted))
    }

    /// An operator whose receiver implements its trait at several right-hand
    /// types the right operand does not tell apart. `admitted` is what the
    /// selection scan already collected, so the report costs no second lookup.
    /// Silent below two candidates: no impl at all is the caller's "operator not
    /// applicable" error.
    pub(super) fn report_ambiguous_operator_rhs(
        &self,
        admitted: &[ArithmeticTraitInfo],
        base_type_id: TypeId,
        op: BinaryOp,
        span: Span,
    ) {
        if admitted.len() < 2 {
            return;
        }
        let tt = self.tysys.type_table.borrow();
        let candidates = admitted
            .iter()
            // The recorded name already carries the impl's own argument
            // (`Add<Feet>`), so render the head with the right-hand type: a
            // defaulted `Rhs = Self` impl then reads like a written one.
            .map(|info| match info.rhs_type {
                Some(t) => format!(
                    "{}<{}>",
                    info.trait_name.base_name(),
                    tt.type_name(tt.peel_refs(t))
                ),
                None => info.trait_name.base_name().to_string(),
            })
            .collect();
        let type_name = tt.type_name(base_type_id);
        drop(tt);
        let _ = self.emit(TypeError::AmbiguousOperatorRhs {
            op: crate::unparse::binary_op_str(op).to_string(),
            type_name,
            candidates,
            span,
        });
    }

    /// Find the self type for an operator trait, given the rhs type.
    /// Used to determine what type a literal lhs should be coerced to.
    /// For most operators, the self type is the same struct type as rhs.
    pub(super) fn find_operator_self_type(
        &mut self,
        rhs_type_id: TypeId,
        op: &BinaryOp,
    ) -> Option<TypeId> {
        let struct_name = self.tysys.struct_name_for_type(rhs_type_id)?;
        let (item, method_name) = super::tysys::operator_trait_method(op)?;
        let trait_ = self.tysys.compiler_trait_def(item)?;
        // `1 + m` reads the impl on the right operand's type and gives the
        // literal that same type, so the impl must be the one whose right-hand
        // type is it — an `Add<Feet>` on `Meters` does not answer for `1 + m`.
        let rhs = ArgClass::Exact(rhs_type_id);
        self.find_arithmetic_trait_impl(
            &struct_name,
            rhs_type_id,
            trait_,
            method_name,
            Some(&rhs),
        )?;
        Some(rhs_type_id)
    }
}

impl TypeSystem {
    /// `Some(struct_type)` when `struct_name` is a non-generic struct whose
    /// fields all declare a default, making it eligible for auto-derived
    /// `Default::default()`. `None` for an unknown name, a required field, no
    /// fields at all, or a generic struct. Does not check for a user-written
    /// `impl Default`, so consult it only as a fallback after the regular
    /// impl-lookup paths.
    pub(super) fn auto_derive_default_struct_type(
        &self,
        scope: &TypeLookup,
        struct_name: &str,
    ) -> Option<TypeId> {
        let info = scope.struct_fields(struct_name)?;
        if info.fields.is_empty() || !info.field_defaults.iter().all(Option::is_some) {
            return None;
        }
        if !info.type_param_type_ids.is_empty() {
            return None;
        }
        Some(self.type_table.borrow().type_id_of_decl(info.defined_at))
    }
}

impl<H: CompilerHost> Elaborator<'_, H> {
    /// The module declaring the type a rendered head names, for a caller whose
    /// receiver carries no declaration. The frame derivation and nothing wider
    /// (WEP 2026-08-12), so an unseen declaration lands where the walk stands.
    pub(super) fn declaring_module_of(&self, struct_name: &str) -> ModuleSource {
        // Primitive impl blocks live in `core:prelude/primitive.wado`. i128 /
        // u128 are structs in `prelude/int128.wado`, not primitives.
        if matches!(
            struct_name,
            "i8" | "i16"
                | "i32"
                | "i64"
                | "u8"
                | "u16"
                | "u32"
                | "u64"
                | "f32"
                | "f64"
                | "bool"
                | "char"
        ) {
            return ModuleSource::primitive();
        }
        if let Some(def) = self.decl_key_or_local(struct_name) {
            return self.tysys.resolutions.defs().module(def).clone();
        }
        // A newtype or `flags` type this walk interned: its `ResolvedType`
        // carries the declaration, so the module comes off that.
        if let Some(type_id) = self.lookup_newtype(struct_name) {
            let declared = match self.tysys.type_table.borrow().get(type_id).clone() {
                ResolvedType::Newtype { def, .. } | ResolvedType::Flags { def } => {
                    Some(self.tysys.type_table.borrow().def_module(def).clone())
                }
                _ => None,
            };
            if let Some(module_source) = declared {
                return module_source;
            }
        }
        self.current_module_source.clone()
    }

    /// Look up method info based on receiver type and method name.
    /// Returns `MethodInfo` including return type and `self_kind`, or None if not found.
    pub(super) fn lookup_method_info(
        &mut self,
        receiver_type: TypeId,
        method_name: &str,
    ) -> Option<MethodInfo> {
        // First, get the base (non-reference) type for method lookup
        let base_type_id = self.tysys.get_base_type(receiver_type);
        self.lookup_method_info_uncached(base_type_id, method_name)
    }

    fn lookup_method_info_uncached(
        &mut self,
        base_type_id: TypeId,
        method_name: &str,
    ) -> Option<MethodInfo> {
        let base_type = self.tysys.type_table.borrow().get(base_type_id).clone();

        // Get the struct name, module source, and type args from the base type
        // For primitives, module_source is None to trigger "search all loaded modules" logic
        let (struct_name, struct_module_source, receiver_type_args, newtype_base) = match &base_type
        {
            ResolvedType::Struct { .. } | ResolvedType::Resource { .. } => {
                let (name, module_source) = self
                    .tysys
                    .type_table
                    .borrow()
                    .nominal_head(base_type_id)
                    .expect("a nominal type names a declaration");
                (name, Some(module_source), None, None)
            }
            // Generic instances like Box<i32> use the base name "Box" for method lookup.
            ResolvedType::GenericInstance { def, type_args } => {
                let name = &self.tysys.type_table.borrow().def_name(*def).to_string();
                let module_source = &self.tysys.type_table.borrow().def_module(*def).clone();
                if TypeTable::is_tuple_type(name) {
                    let elems = type_args;
                    if method_name == "len" {
                        return Some(MethodInfo {
                            method_ast_id: None,
                            return_type: TypeTable::I32,
                            self_kind: ast::SelfKind::Ref,
                            param_types: vec![],
                            param_is_mut: vec![],
                            owner: MethodOwner::Receiver,
                            cm_name: None,
                            is_ref_impl: false,
                            method_type_param_ids: vec![],
                            method_own_params: vec![],
                            impl_module: None,
                            from_concrete_impl: false,
                            param_defaults: vec![],
                            param_names: vec![],
                            consumes_self: false,
                        });
                    }
                    if method_name == "zip" {
                        if elems.is_empty() {
                            return None;
                        }
                        let inner_arities: Vec<Vec<TypeId>> = elems
                            .iter()
                            .filter_map(|e| self.tysys.type_table.borrow().as_tuple(*e))
                            .collect();
                        if inner_arities.len() != elems.len() {
                            return None;
                        }
                        let arity = inner_arities[0].len();
                        if !inner_arities.iter().all(|a| a.len() == arity) {
                            return None;
                        }
                        let mut transposed = Vec::with_capacity(arity);
                        for col in 0..arity {
                            let col_types: Vec<TypeId> =
                                inner_arities.iter().map(|row| row[col]).collect();
                            let col_tuple =
                                self.tysys.type_table.borrow_mut().make_tuple(col_types);
                            transposed.push(col_tuple);
                        }
                        let return_type = self.tysys.type_table.borrow_mut().make_tuple(transposed);
                        return Some(MethodInfo {
                            method_ast_id: None,
                            return_type,
                            self_kind: ast::SelfKind::Ref,
                            param_types: vec![],
                            param_is_mut: vec![],
                            owner: MethodOwner::Receiver,
                            cm_name: None,
                            is_ref_impl: false,
                            method_type_param_ids: vec![],
                            method_own_params: vec![],
                            impl_module: None,
                            from_concrete_impl: false,
                            param_defaults: vec![],
                            param_names: vec![],
                            consumes_self: false,
                        });
                    }
                    (
                        TypeTable::TUPLE_TYPE_NAME.to_string(),
                        None,
                        Some(elems.clone()),
                        None,
                    )
                } else {
                    (
                        name.clone(),
                        Some(module_source.clone()),
                        if type_args.is_empty() {
                            None
                        } else {
                            Some(type_args.clone())
                        },
                        None,
                    )
                }
            }
            // Newtype: first try looking up methods on the newtype itself,
            // then fall back to the base type for method inheritance
            ResolvedType::Newtype {
                base_type,
                type_args: newtype_args,
                ..
            } => {
                let (name, module_source) = &self
                    .tysys
                    .type_table
                    .borrow()
                    .nominal_head(base_type_id)
                    .expect("a newtype names a declaration");
                // Not the base's: a base may re-shape them, and the `impl`
                // header names the newtype.
                let own_type_args = (!newtype_args.is_empty()).then(|| newtype_args.clone());
                let head = name.clone();
                (
                    head,
                    Some(module_source.clone()),
                    own_type_args,
                    Some(*base_type),
                )
            }
            // Flags: first try looking up methods on the flags type itself,
            // then fall back to u32 for method inheritance
            ResolvedType::Flags { .. } => (
                self.tysys
                    .type_table
                    .borrow()
                    .nominal_head(base_type_id)
                    .expect("a flags type names a declaration")
                    .0,
                Some(
                    self.tysys
                        .type_table
                        .borrow()
                        .nominal_head(base_type_id)
                        .expect("a flags type names a declaration")
                        .1,
                ),
                None,
                Some(TypeTable::U32),
            ),
            // Primitive types - search for impl blocks in loaded modules
            // (e.g., impl i32 { fn to_string(&self) -> String { ... } })
            ResolvedType::Primitive(prim) => {
                // Use None to trigger "search all loaded modules" logic
                (prim.as_str().to_string(), None, None, None)
            }
            // Raw GC array `Array<T>` — methods live in `impl Array<T>`
            // (core:prelude/array.wado), keyed by the base name "Array".
            // `None` module triggers "search all loaded modules".
            ResolvedType::BuiltinArray(elem) => (
                TypeTable::ARRAY_TYPE_NAME.to_string(),
                None,
                Some(vec![*elem]),
                None,
            ),
            // Unit type () - search for impl blocks in loaded modules
            ResolvedType::Unit => (TypeTable::UNIT_TYPE_NAME.to_string(), None, None, None),
            // Enum types - search for impl blocks by enum name
            ResolvedType::Enum { .. } => {
                let (name, module_source) = self
                    .tysys
                    .type_table
                    .borrow()
                    .nominal_head(base_type_id)
                    .expect("an enum names a declaration");
                (name, Some(module_source), None, None)
            }
            // Generic resource types (Future<T>, Stream<T>, etc.)
            ResolvedType::GenericResource { def, type_args } => (
                self.tysys.type_table.borrow().def_name(*def).to_string(),
                Some(self.tysys.type_table.borrow().def_module(*def).clone()),
                if type_args.is_empty() {
                    None
                } else {
                    Some(type_args.clone())
                },
                None,
            ),
            _ => return None,
        };

        // Coherence lets any same-package module host an `impl <struct_name>`.
        if struct_module_source.is_some() {
            let entries: Vec<(ModuleSource, AstId)> = self.tysys.trait_env.inherent_impl_keys(
                &self.impl_target_of(base_type_id, &crate::name::DeclName::new(&struct_name)),
            );
            // The receiver's own declaration, which is what an impl header
            // targeting it must name.
            let receiver_decl = self.tysys.type_table.borrow().nominal_def(base_type_id);
            for (impl_module, item_id) in &entries {
                let impl_ref = ImplBlockRef(impl_module.clone(), *item_id);
                let trait_env = Arc::clone(&self.tysys.trait_env);
                let header = impl_header(&trait_env, &impl_ref);
                // The header names its target at a site of its own, answered
                // in the impl's module — so "does this impl target the
                // receiver" is one comparison of declarations rather than a
                // spelling match plus a second lookup asking that module what
                // the spelling means there.
                let header_decl = crate::resolve::head_site(&header.ty)
                    .and_then(|site| self.tysys.resolutions.declared(site));
                let targets_receiver = match (header_decl, receiver_decl) {
                    (Some(header), Some(receiver)) => header == receiver,
                    // A target that names no declaration — a tuple, a function
                    // type — has only its spelling to be compared by.
                    _ => self.get_type_name(&header.ty) == struct_name,
                };
                if !targets_receiver {
                    continue;
                }
                if !self.inherent_impl_applies(header, receiver_type_args.as_deref()) {
                    continue;
                }
                if let Some(info) =
                    self.inherent_method_info(&impl_ref, method_name, receiver_type_args.as_deref())
                {
                    return Some(info);
                }
            }
        }

        if struct_module_source.is_none() {
            let entries: Vec<(ModuleSource, AstId)> = self.tysys.trait_env.inherent_impl_keys(
                &self.impl_target_of(base_type_id, &crate::name::DeclName::new(&struct_name)),
            );
            for (search_module_source, item_id) in &entries {
                let impl_ref = ImplBlockRef(search_module_source.clone(), *item_id);
                let trait_env = Arc::clone(&self.tysys.trait_env);
                let header = impl_header(&trait_env, &impl_ref);
                if self.get_type_name(&header.ty) != struct_name
                    || !self.inherent_impl_applies(header, receiver_type_args.as_deref())
                {
                    continue;
                }
                if let Some(info) =
                    self.inherent_method_info(&impl_ref, method_name, receiver_type_args.as_deref())
                {
                    return Some(info);
                }
            }
        }

        // Instance methods declared on a resource. A resource receiver's
        // `ResolvedType::Resource` / `GenericResource` always carries the
        // resource's defining `module_source` (resolved through imports,
        // re-export chains included), so the method is found in that module
        // directly — no global scan. `None`-module receivers (primitives,
        // `Array`, `()`, tuples) are never resources, so nothing falls through
        // to a scan (issue #1416).
        let receiver_decl = self.tysys.type_table.borrow().nominal_def(base_type_id);
        if let Some(def) = receiver_decl
            && let Some(info) =
                self.find_resource_method_info(def, method_name, receiver_type_args.as_deref())
        {
            return Some(info);
        }

        // For newtypes: if method not found on the newtype itself, try the base type
        // This enables method inheritance: Location (newtype of Point) can use Point's methods
        if let Some(base_type_id) = newtype_base {
            if let Some(mut method_info) = self.lookup_method_info(base_type_id, method_name) {
                // Mark that this method was inherited from the base type
                // This enables proper type checking (e.g., Point::add expects &Point,
                // but when called on Location, it should expect &Location)
                // Only set if not already set (for chained newtypes like C -> B -> A -> Point,
                // we want to keep the innermost base type where the method is defined)
                if method_info.owner == MethodOwner::Receiver {
                    method_info.owner = MethodOwner::InheritedFrom(base_type_id);
                }
                return Some(method_info);
            }
            return None;
        }

        None
    }

    /// Whether an inherent `impl` block accepts a receiver with these type
    /// arguments — its target's concrete positions must match, and its type
    /// parameters' bounds must hold.
    fn inherent_impl_applies(
        &mut self,
        header: &ImplHeader,
        receiver_type_args: Option<&[TypeId]>,
    ) -> bool {
        self.tysys
            .inherent_impl_type_args_match(&header.ty, receiver_type_args)
            && self.tysys.check_impl_block_bounds(
                &self.annotate_ctx,
                &self.type_lookup(),
                &header.type_params,
                &header.ty,
                receiver_type_args,
            )
    }

    /// `MethodInfo` for `method_name` on the inherent `impl` block at
    /// `impl_ref`, or `None` when the block declares no such method.
    fn inherent_method_info(
        &mut self,
        impl_ref: &ImplBlockRef,
        method_name: &str,
        receiver_type_args: Option<&[TypeId]>,
    ) -> Option<MethodInfo> {
        let trait_env = Arc::clone(&self.tysys.trait_env);
        let signatures = Rc::clone(&self.tysys.signatures);
        let header = impl_header(&trait_env, impl_ref);
        let method_header = header.methods.iter().find(|m| m.name == method_name)?;
        let sig = signatures.method_sig(method_header.ast_id)?;
        let impl_sig = signatures
            .impl_sig(impl_ref.1)
            .expect("the decl pass records every impl block's declaration facts");

        let slots = impl_sig.slots(&self.tysys.type_table, receiver_type_args.unwrap_or(&[]));
        let instantiated = sig.decl.instantiate_slots(&self.tysys.type_table, &slots);
        let first_value = sig.first_value_param().min(instantiated.param_types.len());

        Some(MethodInfo {
            method_ast_id: Some(sig.ast_id),
            return_type: instantiated.return_type,
            self_kind: sig.self_kind,
            param_types: instantiated.param_types[first_value..].to_vec(),
            param_is_mut: super::sig::Param::is_mut_flags(&sig.params),
            owner: MethodOwner::Receiver,
            cm_name: None,
            is_ref_impl: false,
            method_type_param_ids: sig.own_type_param_ids(),
            method_own_params: sig.own_params.clone(),
            impl_module: Some(impl_ref.0.clone()),
            from_concrete_impl: self.impl_is_concrete_instantiation(&header.ty),
            param_defaults: sig.params.iter().map(|p| p.default.clone()).collect(),
            param_names: super::sig::Param::names(&sig.params),
            consumes_self: sig.self_kind == ast::SelfKind::Value,
        })
    }

    /// The signature of `method_name` as an instance method on the resource
    /// `def` declares.
    ///
    /// The receiver's `ResolvedType` carries the declaration, so the method is
    /// found on it directly — no name, no module, and so no scan (issue #1416).
    fn find_resource_method_info(
        &mut self,
        def: crate::defs::DefId,
        method_name: &str,
        receiver_type_args: Option<&[TypeId]>,
    ) -> Option<MethodInfo> {
        // Walk the `extends` chain: the nearest declaration answers, and a
        // parent's method keeps the parent's own signature — `Self` included.
        // A generic parent is rejected at declaration, so only the receiver's
        // own declaration takes type arguments.
        let mut current = def;
        let mut args = receiver_type_args;
        loop {
            if let Some(info) = self.resource_method_info_on(current, method_name, args) {
                return Some(info);
            }
            current = self.tysys.type_table.borrow().resource_parent(current)?;
            args = None;
        }
    }

    /// The resource that declares `method_name` for a receiver declared by
    /// `def` — itself or the nearest ancestor. The ambiguity check needs the
    /// declaration, not just the fact, to name it in the diagnostic.
    pub(super) fn resource_declaring(
        &self,
        def: crate::defs::DefId,
        method_name: &str,
    ) -> Option<crate::defs::DefId> {
        let mut current = def;
        loop {
            if let Some(info) = self.tysys.all_resource_types.get(&current)
                && self
                    .tysys
                    .signatures
                    .resource_method_sig(info.defined_at, method_name)
                    .is_some()
            {
                return Some(current);
            }
            current = self.tysys.type_table.borrow().resource_parent(current)?;
        }
    }

    /// The trait whose impl for `type_key` declares `method_name`, if one does.
    /// Direct impls only: a blanket impl answers for every type, so counting it
    /// here would make every prelude-provided name collide.
    pub(super) fn trait_impl_declaring(
        &self,
        type_key: &ImplTargetKey,
        method_name: &str,
    ) -> Option<String> {
        for impl_ref in self.collect_trait_impl_refs_multi(std::slice::from_ref(type_key)) {
            let header = self
                .tysys
                .trait_env
                .impl_headers
                .get(&(impl_ref.0.clone(), impl_ref.1))?;
            if header.methods.iter().any(|m| m.name == method_name)
                && let Some(trait_name) = &header.trait_name
            {
                return Some(trait_name.clone());
            }
        }
        None
    }

    /// [`Self::find_resource_method_info`] for one declaration, without the
    /// `extends` walk.
    fn resource_method_info_on(
        &mut self,
        def: crate::defs::DefId,
        method_name: &str,
        receiver_type_args: Option<&[TypeId]>,
    ) -> Option<MethodInfo> {
        let decl_id = self.tysys.all_resource_types.get(&def)?.defined_at;
        let sig = self
            .tysys
            .signatures
            .resource_method_sig(decl_id, method_name)?
            .clone();
        if sig.self_kind == ast::SelfKind::None {
            return None;
        }

        let instantiated = sig
            .decl
            .instantiate(&self.tysys.type_table, receiver_type_args.unwrap_or(&[]));
        let first_value = sig.first_value_param().min(instantiated.param_types.len());
        let method_type_param_ids = sig.own_type_param_ids();

        Some(MethodInfo {
            method_ast_id: Some(sig.ast_id),
            return_type: instantiated.return_type,
            self_kind: sig.self_kind,
            param_types: instantiated.param_types[first_value..].to_vec(),
            param_is_mut: super::sig::Param::is_mut_flags(&sig.params),
            owner: MethodOwner::Receiver,
            cm_name: sig.cm_name,
            is_ref_impl: false,
            method_type_param_ids,
            method_own_params: sig.own_params.clone(),
            impl_module: None,
            from_concrete_impl: false,
            param_defaults: sig.params.iter().map(|p| p.default.clone()).collect(),
            param_names: super::sig::Param::names(&sig.params),
            consumes_self: sig.self_kind == ast::SelfKind::Value,
        })
    }

    /// The index the declaration gave the first of `slots`, or 0 when there
    /// are none. A slot carries its own index; nothing else knows it.
    fn slot_base(&self, slots: &[TypeId]) -> u32 {
        let table = self.tysys.type_table.borrow();
        slots
            .first()
            .and_then(|&slot| match table.get(slot) {
                ResolvedType::TypeParam { index, .. } | ResolvedType::TypePack { index, .. } => {
                    Some(*index)
                }
                _ => None,
            })
            .unwrap_or(0)
    }

    /// Bind a still-unbound method type param to its declared default,
    /// resolving the default with `Self` set to the concrete receiver and
    /// `default_scope_module` pointed at the declaring module — a default may
    /// name a type private to that module (`<T = Priv>`), which the call site
    /// cannot resolve. The free-function path does the same
    /// ([`Self::fill_defaulted_fn_type_args`]).
    fn fill_defaulted_method_type_args(
        &mut self,
        method_type_params: &[ast::GenericParam],
        receiver_type: TypeId,
        trait_decl: Option<crate::defs::DefId>,
        slots: &[TypeId],
        declaring_module: Option<ModuleSource>,
        inferred: &mut [TypeId],
    ) {
        let receiver_type = self.tysys.get_base_type(receiver_type);
        if self.is_unbound_type_param(receiver_type) {
            return;
        }
        let has_fillable = method_type_params
            .iter()
            .zip(inferred.iter())
            .any(|(p, &tid)| p.default.is_some() && self.is_unbound_type_param(tid));
        if !has_fillable {
            return;
        }
        if let Some(trait_) = trait_decl {
            self.register_assoc_types_for_concrete_type_and_trait(receiver_type, trait_);
        }
        // Re-registering the parameters gives a default like `= T` a scope to
        // resolve against. Number them from the index the declaration gave the
        // first slot — read off the slot, not counted from the receiver's type
        // arguments, which overshoots on a concrete or pack-bearing impl.
        let base = self.slot_base(slots);
        let defaults: Vec<Option<TypeId>> = self.with_self_type(receiver_type, |s| {
            s.with_default_scope_module(declaring_module, |s| {
                let mut scope = s.enter_inherited_type_param_scope();
                scope.annotate_ctx.trait_ctx.type_params.clear();
                scope.register_generic_params(method_type_params, base);
                method_type_params
                    .iter()
                    .map(|p| p.default.as_ref().map(|ty| scope.resolve_type(ty)))
                    .collect()
            })
        });
        for i in 0..inferred.len() {
            if self.is_unbound_type_param(inferred[i])
                && let Some(default_ty) = defaults[i]
                && default_ty != TypeTable::ERROR
                && !self
                    .tysys
                    .type_table
                    .borrow()
                    .contains_type_param(default_ty)
            {
                inferred[i] = default_ty;
            }
        }
    }

    /// Infer an instance call's method-level type arguments from the method's
    /// already-resolved parameter and return types, which must come from a method
    /// lookup so their slots are the ones the caller binds. Deliberately does not
    /// re-resolve the method's AST: a fresh scope would report spurious errors for
    /// a `Self::Item`. An unbound parameter keeps its `TypeParam` id.
    pub(super) fn infer_method_type_args(
        &mut self,
        input: MethodInferenceInput<'_>,
    ) -> Vec<TypeId> {
        let MethodInferenceInput {
            receiver_type,
            method_name,
            slots,
            own_params,
            param_types,
            args,
            raw_args,
            decl_return_type,
            expected_return_type,
            trait_decl,
            declaring_module,
            span,
        } = input;

        // The declaration dispatch selected, in both its forms: the slots to
        // solve, and the parameters that wrote them. They are parallel by
        // construction, so nothing here looks the method up again — a name
        // scan cannot tell which declaration was chosen, and answered with an
        // unrelated trait's same-named method.
        let method_type_params = own_params.to_vec();
        if method_type_params.is_empty() {
            return vec![];
        }

        let inst = self.instantiate(
            slots,
            &Instantiation {
                kind: "method",
                name: method_name,
                span,
            },
        );
        self.record_slot_bounds(&inst, &method_type_params, span);
        let param_types = self.instantiate_types(param_types, &inst);
        let decl_return_type = self.instantiate_type(decl_return_type, &inst);

        let mut infer = InferCtx::new(&self.tysys.type_table, inst.vars.clone());
        for (i, (&param_type, arg)) in param_types.iter().zip(args.iter()).enumerate() {
            if Self::is_literal_number_arg(raw_args.get(i)) {
                infer.add_deferred(param_type, arg.type_id);
            } else {
                infer.add(param_type, arg.type_id);
            }
        }
        if let Some(expected) = expected_return_type {
            infer.add_expected_return(decl_return_type, expected);
        }

        let mut inferred = infer.solve();
        // Resolve method type params that appear only inside another method
        // param's associated-type-equality bound (e.g.
        // `fn m<T, I: Iterator<Item = T>>`), mirroring the free-function path.
        self.resolve_assoc_bound_args(&method_type_params, &mut inferred);
        self.fill_defaulted_method_type_args(
            &method_type_params,
            receiver_type,
            trait_decl,
            slots,
            declaring_module,
            &mut inferred,
        );
        // A slot the solver left as its own variable is unconstrained. The
        // variable already carries the "cannot infer" diagnostic and the
        // module-end sweep, so nothing needs classifying here: what an
        // enclosing generic happens to be named cannot be confused with it.
        self.record_instantiation(&inst, &inferred);
        self.blame_unsolved(&inst, &inferred);
        inferred
    }

    /// Reject a `&mut self` method call whose receiver place is rooted at an
    /// immutable reference: the callee's writes would land in a temporary
    /// copy, never in the storage the caller reads. Mirrors the
    /// assignment-side "cannot assign through immutable reference" rule. A
    /// non-place receiver (call result, literal) stays legal — mutating an
    /// owned temporary is well-defined.
    pub(super) fn check_mut_receiver(
        &mut self,
        receiver: &TirExpr,
        receiver_ast: Option<&ast::Expr>,
        method_name: &str,
        span: Span,
        ctx: &FunctionContext,
    ) {
        let immutable = match self.tysys.type_table.borrow().get(receiver.type_id) {
            ResolvedType::Ref(_) => true,
            ResolvedType::MutRef(_) => false,
            _ => receiver_ast.is_some_and(|e| self.place_roots_at_immutable_ref(e)),
        };
        if immutable {
            let _ = self.emit(TypeError::CannotMutate {
                message: format!(
                    "cannot call `&mut self` method `{method_name}` through immutable reference"
                ),
                span,
            });
            return;
        }
        if let Some(binding) =
            receiver_ast.and_then(|e| self.place_roots_at_immutable_binding(e, ctx))
        {
            let _ = self.emit(TypeError::CannotMutate {
                message: format!(
                    "cannot call `&mut self` method `{method_name}` on immutable '{binding}'"
                ),
                span,
            });
        }

        // `operators.rs` refuses the explicit `&mut x.f`; the implicit receiver
        // borrow must match it.
        if receiver_ast
            .is_some_and(|e| matches!(e, ast::Expr::FieldAccess(_) | ast::Expr::Index(_)))
            && self.is_replace_on_assign_place_type(receiver.type_id)
        {
            let _ = self.emit(TypeError::CannotMutate {
                message: format!(
                    "cannot call `&mut self` method `{method_name}` on {REPLACE_ON_ASSIGN_PLACE}"
                ),
                span,
            });
        }
    }

    /// A type nothing survives a `&mut` copy of — primitive, enum, flags, or
    /// fn, or a newtype over one. A `variant` is excluded: its payload is a
    /// shared GC struct, so mutation through the payload lands.
    pub(super) fn is_replace_on_assign_place_type(&self, type_id: TypeId) -> bool {
        let table = self.tysys.type_table.borrow();
        let replaces_on_assign = |ty: &ResolvedType| {
            matches!(
                ty,
                ResolvedType::Primitive(_)
                    | ResolvedType::Enum { .. }
                    | ResolvedType::Function { .. }
            )
        };
        if replaces_on_assign(table.get(type_id)) {
            return true;
        }
        let base = table.get_ultimate_base_type(type_id);
        replaces_on_assign(table.get(base))
    }

    /// The immutable binding a place roots at: `x`, `x.f`, `x[i]`, `*x`, and
    /// any nesting of those. A reference step ends the walk; `&T` is
    /// [`Self::place_roots_at_immutable_ref`]'s to report.
    pub(super) fn place_roots_at_immutable_binding(
        &self,
        expr: &ast::Expr,
        ctx: &FunctionContext,
    ) -> Option<String> {
        if let Some(ty) = self.sem.types.expression_types.get(&expr.id()).copied()
            && matches!(
                self.tysys.type_table.borrow().get(ty),
                ResolvedType::Ref(_) | ResolvedType::MutRef(_)
            )
        {
            return None;
        }
        match expr {
            ast::Expr::Ident(id) => match binding_mutability(&id.name, ctx) {
                Some(is_mut) => (!is_mut).then(|| id.name.clone()),
                // Only a name no binding claims can be the global; one
                // shadowing it answers for itself.
                None => self.is_immutable_global(&id.name).then(|| id.name.clone()),
            },
            ast::Expr::FieldAccess(fa) => self.place_roots_at_immutable_binding(&fa.expr, ctx),
            ast::Expr::Index(ix) => self.place_roots_at_immutable_binding(&ix.expr, ctx),
            ast::Expr::Unary(u) if u.op == ast::UnaryOp::Deref => {
                self.place_roots_at_immutable_binding(&u.expr, ctx)
            }
            _ => None,
        }
    }

    /// Whether the place `expr` reaches its storage through an immutable
    /// reference (`&T`): a field/index/deref chain whose crossed step has
    /// recorded type `&T`. A `&mut T` step makes the place mutable and stops
    /// the walk. Works on the source AST plus recorded `expression_types`
    /// because annotate-time receiver TIR is a placeholder without structure.
    pub(super) fn place_roots_at_immutable_ref(&self, expr: &ast::Expr) -> bool {
        let inner = match expr {
            ast::Expr::FieldAccess(fa) => &fa.expr,
            ast::Expr::Index(ix) => &ix.expr,
            ast::Expr::Unary(u) if u.op == ast::UnaryOp::Deref => &u.expr,
            _ => return false,
        };
        let Some(inner_type) = self.sem.types.expression_types.get(&inner.id()).copied() else {
            return false;
        };
        match self.tysys.type_table.borrow().get(inner_type) {
            ResolvedType::Ref(_) => true,
            ResolvedType::MutRef(_) => false,
            _ => self.place_roots_at_immutable_ref(inner),
        }
    }

    /// Adjust the receiver expression to match what the method's self parameter expects.
    ///
    /// When `is_ref_impl` is true, the method was found on a reference type impl
    /// (e.g., `impl Trait for &T`). In this case, Self is `&T`, so `&self` means `&&T`.
    /// The receiver (which is `&T`) needs an additional `&` wrapping.
    pub(super) fn adjust_receiver_for_self_kind(
        &mut self,
        receiver: TirExpr,
        self_kind: ast::SelfKind,
        is_ref_impl: bool,
        span: Span,
    ) -> TirExpr {
        Self::adjust_receiver_for_self_kind_static(
            receiver,
            self_kind,
            is_ref_impl,
            span,
            &self.tysys.type_table,
        )
    }

    /// `&TypeTable`-only version of [`Self::adjust_receiver_for_self_kind`]
    /// — [`super::reify::Reify`] calls this directly so it can
    /// reproduce the receiver adjustment from the recorded
    /// `(self_kind, is_ref_impl)` pair without holding an [`Elaborator`].
    /// The instance method above stays as a thin delegate so existing
    /// elaborator callers don't need to change.
    pub(super) fn adjust_receiver_for_self_kind_static(
        receiver: TirExpr,
        self_kind: ast::SelfKind,
        is_ref_impl: bool,
        span: Span,
        type_table: &std::cell::RefCell<crate::tir::TypeTable>,
    ) -> TirExpr {
        if is_ref_impl {
            // For ref-type impls, Self is &T (or &mut T).
            // &self means &&T, &mut self means &mut &T.
            // The receiver is already &T, so we need to add an extra reference layer.
            return match self_kind {
                ast::SelfKind::Ref => {
                    let ref_type = type_table.borrow_mut().make_ref(receiver.type_id);
                    TirExpr::new(
                        TirExprKind::Unary {
                            op: TirUnaryOp::Ref,
                            expr: Box::new(receiver),
                        },
                        ref_type,
                        span,
                    )
                }
                ast::SelfKind::MutRef => {
                    let mut_ref_type = type_table.borrow_mut().make_mut_ref(receiver.type_id);
                    TirExpr::new(
                        TirExprKind::Unary {
                            op: TirUnaryOp::MutRef,
                            expr: Box::new(receiver),
                        },
                        mut_ref_type,
                        span,
                    )
                }
                ast::SelfKind::None | ast::SelfKind::Value => {
                    Self::deref_to_value_static(receiver, span, type_table)
                }
            };
        }

        let receiver_type = type_table.borrow().get(receiver.type_id).clone();

        match self_kind {
            ast::SelfKind::None | ast::SelfKind::Value => {
                // No auto-ref: static method context, or a by-value `self`
                // receiver that transfers the resource. Deref all refs.
                Self::deref_to_value_static(receiver, span, type_table)
            }
            ast::SelfKind::Ref => {
                // Method expects &self
                match &receiver_type {
                    ResolvedType::Ref(_) => {
                        // Already &T, use as-is
                        receiver
                    }
                    ResolvedType::MutRef(_) => {
                        // &mut T can be coerced to &T, use as-is
                        receiver
                    }
                    _ => {
                        // Value T, need to add &
                        let ref_type = type_table.borrow_mut().make_ref(receiver.type_id);
                        TirExpr::new(
                            TirExprKind::Unary {
                                op: TirUnaryOp::Ref,
                                expr: Box::new(receiver),
                            },
                            ref_type,
                            span,
                        )
                    }
                }
            }
            ast::SelfKind::MutRef => {
                // Method expects &mut self
                if let ResolvedType::MutRef(_) = &receiver_type {
                    // Already &mut T, use as-is
                    receiver
                } else {
                    // Value T, need to add &mut
                    let mut_ref_type = type_table.borrow_mut().make_mut_ref(receiver.type_id);
                    TirExpr::new(
                        TirExprKind::Unary {
                            op: TirUnaryOp::MutRef,
                            expr: Box::new(receiver),
                        },
                        mut_ref_type,
                        span,
                    )
                }
            }
        }
    }

    /// `&TypeTable`-only version of the receiver-deref loop, paired
    /// with [`Self::adjust_receiver_for_self_kind_static`] for reify's reuse.
    pub(super) fn deref_to_value_static(
        mut receiver: TirExpr,
        span: Span,
        type_table: &std::cell::RefCell<crate::tir::TypeTable>,
    ) -> TirExpr {
        loop {
            match type_table.borrow().get(receiver.type_id).clone() {
                ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => {
                    receiver = TirExpr::new(
                        TirExprKind::Unary {
                            op: TirUnaryOp::Deref,
                            expr: Box::new(receiver),
                        },
                        inner,
                        span,
                    );
                }
                _ => return receiver,
            }
        }
    }

    /// The typed receiver chain: `type_key` plus the newtype/flags base heads
    /// reachable from it. A reference head has no newtype base, so it is
    /// returned as a singleton; a named head walks its newtype chain via
    /// [`Self::newtype_chain_names`]. Each base is re-canonicalised, since a
    /// newtype's base may be declared in another module.
    fn newtype_chain(&self, type_key: &ImplTargetKey) -> Vec<ImplTargetKey> {
        let Some(name) = type_key.type_name(self.tysys.resolutions.defs()) else {
            return vec![type_key.clone()];
        };
        // The head keeps the caller's key; each base is keyed from the
        // `TypeId` the chain walk already holds, since a base declared in
        // another module cannot be recovered from its name alone.
        let mut chain = vec![type_key.clone()];
        chain.extend(
            self.newtype_chain_bases(name)
                .into_iter()
                .map(|(base_id, base_name)| match base_id {
                    Some(id) => self.impl_target_of(id, &crate::name::DeclName::new(&base_name)),
                    None => self.impl_target(&base_name),
                }),
        );
        chain
    }

    /// `struct_name` plus the base names of its newtype chain
    /// (`type Alias = Base` → `Base`, `Flags` → `u32`), so a trait impl on a
    /// base type is reachable through the alias.
    fn newtype_chain_names(&self, struct_name: &str) -> Vec<String> {
        std::iter::once(struct_name.to_string())
            .chain(
                self.newtype_chain_bases(struct_name)
                    .into_iter()
                    .map(|(_, name)| name),
            )
            .collect()
    }

    /// The newtype chain's bases, each with the `TypeId` it was reached
    /// through. `Flags` widens to `u32`, which is a primitive rather than a
    /// type in the table, so it carries no id.
    fn newtype_chain_bases(&self, struct_name: &str) -> Vec<(Option<TypeId>, String)> {
        let mut bases = Vec::new();
        if let Some(newtype_id) = self.lookup_newtype(struct_name) {
            let mut current = newtype_id;
            loop {
                match self.tysys.type_table.borrow().get(current).clone() {
                    ResolvedType::Newtype { base_type, .. } => {
                        let base_name = match self.tysys.type_table.borrow().get(base_type).clone()
                        {
                            ResolvedType::GenericInstance { def, .. }
                            | ResolvedType::GenericResource { def, .. } => {
                                self.tysys.type_table.borrow().def_name(def).to_string()
                            }
                            ResolvedType::BuiltinArray(_) => TypeTable::ARRAY_TYPE_NAME.to_string(),
                            _ => self.tysys.type_table.borrow().type_name(base_type),
                        };
                        bases.push((Some(base_type), base_name));
                        current = base_type;
                    }
                    ResolvedType::Flags { .. } => {
                        bases.push((None, "u32".to_string()));
                        break;
                    }
                    _ => break,
                }
            }
        }
        bases
    }

    /// Every trait impl on one of `names_to_check`, plus blanket impls
    /// (`impl<T: Bound> Trait for T`) whose bound the receiver satisfies.
    fn trait_method_candidates(
        &mut self,
        names_to_check: &[ImplTargetKey],
        receiver_type_id: Option<TypeId>,
    ) -> Vec<ImplBlockRef> {
        // Collect lightweight impl block references (avoiding deep clones).
        let mut impl_refs = self.collect_trait_impl_refs_multi(names_to_check);

        // Blanket impl fallback: check `impl<T: Bound> Trait for T` where the receiver
        // type satisfies the bound.  e.g., `impl<I: Iterator> IntoIterator for I` matches
        // any concrete type that implements Iterator. Snapshot the value blankets
        // (module, ast id, bound names) so the per-bound checks below borrow `self`
        // without holding a `trait_env` borrow.
        let value_blankets: Vec<(ModuleSource, AstId, Vec<super::trait_env::BlanketBound>)> = self
            .tysys
            .trait_env
            .blanket_impls
            .values()
            .flatten()
            .filter(|b| b.receiver == super::trait_env::BlanketReceiver::Value)
            .map(|b| (b.module.clone(), b.ast_id, b.bounds.clone()))
            .collect();
        let type_lookup = self.type_lookup();
        for (module, ast_id, bounds) in &value_blankets {
            // Gate on all bounds. The receiver-`TypeId` check is preferred:
            // it recognises synthesized bounds (`ReflectStruct`, `Default`) with no
            // explicit `impl`, which the name-based lookup misses. A viable
            // blanket must survive to the authoritative `candidate_matches_receiver`.
            let bounds_satisfied = bounds.iter().all(|bound| {
                let Some(bound_def) = bound.decl_ref else {
                    return false;
                };
                if let Some(rt) = receiver_type_id
                    && self.tysys.type_implements_trait(
                        &self.annotate_ctx,
                        &type_lookup,
                        rt,
                        bound_def,
                    )
                {
                    return true;
                }
                names_to_check.iter().any(|target| {
                    self.tysys.find_trait_impl_for_subject(
                        &self.annotate_ctx,
                        &type_lookup,
                        receiver_type_id,
                        &target.receiver(self.tysys.resolutions.defs()),
                        bound_def,
                    )
                })
            });
            // The bound holding is not the whole condition: a blanket pinning
            // an associated type to its receiver (`T: Mul<Output = T>`) does
            // not apply to one that widens.
            if bounds_satisfied
                && self
                    .tysys
                    .blanket_assoc_constraints_hold(receiver_type_id, bounds)
            {
                impl_refs.push(ImplBlockRef(module.clone(), *ast_id));
            }
        }
        impl_refs
    }

    /// Whether a candidate impl applies to the receiver. Returns its receiver
    /// name and whether it is a blanket type-param impl, or `None` to skip.
    fn candidate_matches_receiver(
        &mut self,
        impl_ref: &ImplBlockRef,
        names_to_check: &[ImplTargetKey],
        receiver_type_id: Option<TypeId>,
    ) -> Option<(String, crate::name::FqTypeName, bool)> {
        let trait_env = Arc::clone(&self.tysys.trait_env);
        let header = impl_header(&trait_env, impl_ref);
        let impl_struct_name = self.get_type_name(&header.ty);
        let impl_key = super::trait_env::receiver_decl_key(&header.ty);
        // Accept if the type matches by name, or if it's a blanket impl type parameter.
        let is_blanket_type_param =
            matches!(&header.ty, Type::Named(named) if !self.tysys.is_known_type_name(&named.name));
        // Head comparison: the chain's targets are canonical, while this key
        // comes straight off the impl's own AST.
        if !names_to_check
            .iter()
            .any(|target| target.receiver(self.tysys.resolutions.defs()).decl_key() == impl_key)
            && !is_blanket_type_param
        {
            return None;
        }
        if !self.ref_impl_targets_receiver(&header.ty, receiver_type_id) {
            return None;
        }
        if !self.blanket_target_bounds_satisfied(&header.ty, &header.type_params, receiver_type_id)
        {
            return None;
        }
        if !self.concrete_impl_matches_receiver(impl_ref, receiver_type_id) {
            return None;
        }
        // Qualified in the impl's own frame by the decl pass: the call site's
        // imports may name the same declaration differently, or not at all.
        let impl_struct_fq = self.impl_sig(impl_ref).target_fq.clone();
        Some((impl_struct_name, impl_struct_fq, is_blanket_type_param))
    }

    /// For a reference-typed impl (`impl ... for &Container<T>`), whether its
    /// inner outer name matches the receiver's. `candidate_matches_receiver`'s
    /// name match only sees `"&"` / `"&mut"` (`get_type_name` collapses every
    /// reference to that literal), so without this check every ref impl would
    /// match any `&T` receiver. Blanket `impl<T: Bound> Trait for &T` (inner is
    /// a bare type-param name) is exempt — soundness handled by the bound check.
    /// Returns `true` (keep) for any non-reference impl.
    fn ref_impl_targets_receiver(&self, impl_ty: &Type, receiver_type_id: Option<TypeId>) -> bool {
        if RefKind::from_ast(impl_ty).is_none() {
            return true;
        }
        let Some(rt) = receiver_type_id else {
            return true;
        };
        let impl_inner_outer = match impl_ty {
            Type::Reference(inner) | Type::MutReference(inner) => match inner.as_ref() {
                Type::Generic(g) => Some(g.name.clone()),
                Type::Named(named) if self.tysys.is_known_type_name(&named.name) => {
                    Some(named.name.clone())
                }
                _ => None, // blanket `&T` form — handled by the bound check
            },
            _ => None,
        };
        let Some(impl_inner) = impl_inner_outer else {
            return true;
        };
        let receiver_outer = match self.tysys.type_table.borrow().get(rt) {
            ResolvedType::GenericInstance { .. }
            | ResolvedType::Struct { .. }
            | ResolvedType::Enum { .. }
            | ResolvedType::Resource { .. }
            | ResolvedType::GenericResource { .. }
            | ResolvedType::Newtype { .. }
            | ResolvedType::Flags { .. }
            | ResolvedType::Variant { .. } => self
                .tysys
                .type_table
                .borrow()
                .nominal_head(rt)
                .map(|(n, _)| n)
                .unwrap_or_default(),
            ResolvedType::Primitive(p) => p.as_str().to_string(),
            // The raw GC array's outer constructor is "Array", so a `&Array<T>`
            // ref impl (`impl Trait for &Array<T>`) matches a `&Array<_>` receiver.
            ResolvedType::BuiltinArray(_) => TypeTable::ARRAY_TYPE_NAME.to_string(),
            // Receivers with no nominal outer name (`TypeParam`, `Unknown`, …)
            // reach here, e.g. `&T`. The empty sentinel never equals the
            // non-empty `impl_inner`, so the ref impl is not matched.
            _ => String::new(),
        };
        if impl_inner != receiver_outer
            && matches!(
                self.tysys.type_table.borrow().get(rt),
                ResolvedType::Newtype { .. }
            )
        {
            return self
                .newtype_chain_names(&receiver_outer)
                .contains(&impl_inner);
        }
        impl_inner == receiver_outer
    }

    /// For a blanket impl (target type is one of its own type params with
    /// bounds), whether the receiver satisfies those bounds. Prevents using e.g.
    /// `impl<I: Iterator> IntoIterator for I` for a `TypeParam` `I` that is not
    /// itself `Iterator`. Returns `true` (keep) for non-blanket impls.
    fn blanket_target_bounds_satisfied(
        &self,
        impl_ty: &Type,
        impl_type_params: &[ast::GenericParam],
        receiver_type_id: Option<TypeId>,
    ) -> bool {
        let impl_ty_name = super::trait_env::get_type_name_static(impl_ty);
        let Some(param) = impl_type_params
            .iter()
            .find(|tp| tp.name == impl_ty_name && !tp.bounds.is_empty())
        else {
            return true;
        };
        param.bounds.iter().all(|bound| {
            let Some(bound_def) = self.bound_trait_def(bound.id) else {
                return true;
            };
            receiver_type_id.is_some_and(|rt| {
                self.tysys.type_implements_trait(
                    &self.annotate_ctx,
                    &self.type_lookup(),
                    rt,
                    bound_def,
                )
            })
        })
    }

    /// Whether a concrete `impl Trait for <NamedType>` really targets the
    /// receiver. `candidate_matches_receiver`'s bare-name check accepts every
    /// same-named impl, so resolve each impl's receiver in its own module and
    /// compare `TypeId`s along the newtype chain. A `TypeParam`-bearing impl is
    /// exempt; a concrete `impl X for List<u8>` is checked, or `List<i32>` matches.
    fn concrete_impl_matches_receiver(
        &mut self,
        impl_ref: &ImplBlockRef,
        receiver_type_id: Option<TypeId>,
    ) -> bool {
        let Some(receiver) = receiver_type_id else {
            return true;
        };
        let trait_env = Arc::clone(&self.tysys.trait_env);
        let header = impl_header(&trait_env, impl_ref);
        // Which target positions are slots is `is_impl_target_param`'s
        // question, asked in the impl's *own* module — the same call
        // `enter_impl_frame` makes when it binds them. Asking it any other way
        // (a module-agnostic "is this a known type name?") disagrees with the
        // frame exactly when a target argument names a type the impl's module
        // cannot see, and the target then carries a slot this filter believes
        // is concrete.
        let is_target_slot = |name: &str| {
            self.tysys
                .is_impl_target_param(&impl_ref.0, &header.type_params, name)
        };
        let is_blanket_tp = matches!(&header.ty, Type::Named(n) if is_target_slot(&n.name));
        let generic_is_parametric = matches!(&header.ty, Type::Generic(g)
            if g.args.iter().any(|a| matches!(a, Type::Named(n) if is_target_slot(&n.name))));
        let skip_filter = !header.type_params.is_empty()
            || is_blanket_tp
            || matches!(&header.ty, Type::Reference(_) | Type::MutReference(_))
            || generic_is_parametric;
        if skip_filter {
            return true;
        }
        // Every abstract target shape is now behind `skip_filter`, so the
        // block's own `Self` is a concrete receiver this impl demands.
        let impl_recv_id = self.impl_sig(impl_ref).self_type;
        debug_assert!(
            !self
                .tysys
                .type_table
                .borrow()
                .contains_type_param(impl_recv_id),
            "a slot-carrying impl target reached the concrete-receiver filter"
        );
        let tt = self.tysys.type_table.borrow();
        let target = tt.peel_refs(impl_recv_id);
        let mut current = tt.peel_refs(receiver);
        loop {
            if current == target {
                return true;
            }
            match tt.get(current) {
                ResolvedType::Newtype { base_type, .. } => {
                    current = tt.peel_refs(*base_type);
                }
                _ => return false,
            }
        }
    }

    /// Find a trait method for a given type and method name, for when an
    /// inherent method is not found.
    ///
    /// `receiver_type_args` should contain the concrete type arguments for
    /// generic receivers (e.g., `[i32]` for `Box_<i32>`), which fill the
    /// declaring impl block's slots.
    pub(super) fn find_trait_method_for_type(
        &mut self,
        type_key: &ImplTargetKey,
        method_name: &str,
        receiver_type_args: Option<&[TypeId]>,
        receiver_type_id: Option<TypeId>,
        span: Span,
        // `required_trait`: set by a trait-qualified call
        // (`Alpha::describe(&x)`), where only impls of that trait's
        // declaration may answer; its `args`, when present (turbofish), pin
        // one argument list.
        required_trait: Option<&super::types::RequiredTrait>,
        // `probe`: the call's arguments, classified on demand to select
        // among one trait's argument lists. `None` from callers with no
        // argument list at hand.
        probe: Option<&mut ArgProbe<'_>>,
    ) -> Option<super::types::TraitMethodMatch> {
        use super::types::TraitMethodMatch;
        let mut found_traits: Vec<TraitMethodMatch> = Vec::new();

        let names_to_check = self.newtype_chain(type_key);
        let impl_refs = self.trait_method_candidates(&names_to_check, receiver_type_id);

        for impl_ref in &impl_refs {
            let Some((impl_struct_name, impl_struct_fq, is_blanket_type_param)) =
                self.candidate_matches_receiver(impl_ref, &names_to_check, receiver_type_id)
            else {
                continue;
            };
            found_traits.extend(self.collect_trait_method_matches_from_impl(
                impl_ref,
                impl_struct_name,
                impl_struct_fq,
                is_blanket_type_param,
                method_name,
                receiver_type_args,
                receiver_type_id,
            ));
        }

        // A qualified call already said which trait it means, so the candidates
        // from every other trait are not competitors — dropping them here is
        // what keeps `select_trait_match` from reporting an ambiguity the call
        // site has resolved by naming one.
        if let Some(wanted) = required_trait {
            found_traits.retain(|m| {
                crate::resolve::Resolution::Def(m.trait_decl) == wanted.decl
                    && wanted
                        .args
                        .as_ref()
                        .is_none_or(|args| &m.trait_args == args)
            });
        }

        if let Some(m) = self.select_trait_match(found_traits, method_name, span, probe) {
            return Some(m);
        }

        // Auto-derived Eq / Ord: no user-written impl exists, but the type
        // satisfies the field-wise / case-wise eligibility rules and
        // `synthesis::traits` will emit a body. Synthesize a `TraitMethodMatch`
        // so method-call resolution (and everything downstream of it) sees
        // the same view of "does this type have `.eq` / `.cmp`?" that
        // operator dispatch gets via `find_eq_trait_impl` / `find_ord_trait_impl`.
        // A qualified call that names some *other* trait (`Same::eq(&a, &b)`)
        // must not be answered by the derived prelude trait.
        if let Some(recv_id) = receiver_type_id
            && required_trait.is_none_or(|wanted| {
                self.tysys
                    .auto_derive_by_method(method_name)
                    .is_some_and(|(item, _, _)| {
                        self.tysys.compiler_trait_def(item).is_some_and(|decl| {
                            crate::resolve::Resolution::Def(decl) == wanted.decl
                        })
                    })
            })
        {
            // A template is registered under its mangled head, so the probe
            // is in that namespace, not the declaration one.
            return self.try_auto_derived_method_match(
                type_key
                    .receiver(self.tysys.resolutions.defs())
                    .head_key()
                    .as_mangled_str(),
                method_name,
                recv_id,
            );
        }
        None
    }

    /// Project one candidate trait impl into 0+ [`TraitMethodMatch`]es: set up
    /// the impl's type-parameter / associated-type scope, then emit a match for
    /// the impl's own `method_name`, or failing that for any matching trait
    /// default method with a body.
    fn collect_trait_method_matches_from_impl(
        &mut self,
        impl_ref: &ImplBlockRef,
        impl_struct_name: String,
        impl_struct_fq: crate::name::FqTypeName,
        is_blanket_type_param: bool,
        method_name: &str,
        receiver_type_args: Option<&[TypeId]>,
        receiver_type_id: Option<TypeId>,
    ) -> Vec<super::types::TraitMethodMatch> {
        use super::types::TraitMethodMatch;
        let mut found_traits: Vec<TraitMethodMatch> = Vec::new();

        // Extract type param mappings from the impl header before mutating self.
        let trait_env = Arc::clone(&self.tysys.trait_env);
        let header = impl_header(&trait_env, impl_ref);
        // Track variadic type pack spreads: (pack_name, param_index)
        let mut variadic_pack_entry: Option<(String, u32)> = None;
        let impl_home = self.impl_block_module_source(impl_ref);
        let target_params = |generic: &ast::GenericType| -> Vec<(String, u32)> {
            generic
                .args
                .iter()
                .enumerate()
                .filter_map(|(i, arg)| match arg {
                    Type::Named(named)
                        if self.tysys.is_impl_target_param(
                            &impl_home,
                            &header.type_params,
                            &named.name,
                        ) =>
                    {
                        Some((named.name.clone(), i as u32))
                    }
                    _ => None,
                })
                .collect()
        };
        let type_param_entries: Vec<(String, u32)> = if let Type::Generic(generic) = &header.ty {
            target_params(generic)
        } else if let Type::Reference(boxed) | Type::MutReference(boxed) = &header.ty {
            if let Type::Named(inner) = boxed.as_ref() {
                // impl<T: Bound> Trait for &T / &mut T: T is at position 0
                vec![(inner.name.clone(), 0u32)]
            } else if let Type::Generic(generic) = boxed.as_ref() {
                // impl<T> Trait for &Container<T>: extract type params from generic args
                target_params(generic)
            } else {
                Vec::new()
            }
        } else if let Type::Tuple(elems) = &header.ty {
            // Handle variadic tuple impls: impl<..T> Trait for [..T]
            // TypePackSpread elements map the pack name to its index.
            let mut entries = Vec::new();
            for (i, elem) in elems.iter().enumerate() {
                match elem {
                    Type::TypePackSpread(name, _) => {
                        // Find the pack's index from the impl block's type_params
                        let pack_idx = header
                            .type_params
                            .iter()
                            .position(|tp| tp.name == *name)
                            .map(|p| p as u32)
                            .unwrap_or(i as u32);
                        variadic_pack_entry = Some((name.clone(), pack_idx));
                    }
                    Type::Named(named) => {
                        entries.push((named.name.clone(), i as u32));
                    }
                    _ => {}
                }
            }
            entries
        } else {
            Vec::new()
        };
        // Extract blanket type param info
        let blanket_name = if let Type::Named(named) = &header.ty {
            Some(named.name.clone())
        } else {
            None
        };
        let impl_module_source = impl_home.clone();
        // A concrete generic instantiation trait impl (`impl Tag for
        // List<u8>`) yields a per-instantiation concrete method, called
        // directly (no monomorphization), living in the impl's module.
        let impl_is_concrete = self.impl_is_concrete_instantiation(&header.ty);

        // Save trait context for this impl block scope. We use an inherited
        // scope (saves the full ctx via clone) and then selectively clear
        // just type_params and assoc_type_bindings — other parts (bounds,
        // self_type, …) are kept from the parent for this lookup.
        let mut scope = self.enter_inherited_type_param_scope();
        scope.annotate_ctx.trait_ctx.type_params.clear();
        scope.annotate_ctx.trait_ctx.assoc_type_bindings.clear();

        // Set up type parameters for resolving generic associated types
        if let Some(type_args) = receiver_type_args {
            for (name, idx) in &type_param_entries {
                let i = *idx as usize;
                if i < type_args.len() {
                    scope
                        .annotate_ctx
                        .trait_ctx
                        .type_params
                        .insert(name.clone(), (*idx, type_args[i]));
                }
            }
            // For variadic pack params (..T in impl<..T> Trait for [..T]),
            // map the pack to a TypePack so that the method body can reference it.
            if let Some((pack_name, pack_idx)) = &variadic_pack_entry {
                let pack_type = scope
                    .tysys
                    .type_table
                    .borrow_mut()
                    .make_tuple(type_args.to_vec());
                scope
                    .annotate_ctx
                    .trait_ctx
                    .type_params
                    .insert(pack_name.clone(), (*pack_idx, pack_type));
            }
        }

        // For blanket impls where impl_ty is a free type parameter
        if let Some(ref name) = blanket_name
            && !scope.annotate_ctx.trait_ctx.type_params.contains_key(name)
            && !scope.tysys.is_known_type_name(name)
        {
            if let Some(recv_id) = receiver_type_id {
                // At the slot the impl gave it, which is 0 only when the
                // receiver is the first parameter written. `impl<A, T:
                // Holder<Item = A>> Trait for T` puts it at 1, and binding it
                // at 0 leaves the target itself unsubstituted.
                let slot = header
                    .type_params
                    .iter()
                    .filter(|p| p.is_real_type_param())
                    .position(|p| &p.name == name)
                    .unwrap_or(0) as u32;
                scope
                    .annotate_ctx
                    .trait_ctx
                    .type_params
                    .insert(name.clone(), (slot, recv_id));
            } else {
                let type_id = scope
                    .tysys
                    .type_table
                    .borrow_mut()
                    .make_type_param(name.clone(), 0);
                scope
                    .annotate_ctx
                    .trait_ctx
                    .type_params
                    .insert(name.clone(), (0, type_id));
            }
        }

        // The receiver's type arguments, aligned to the impl's slots per its
        // shape (generic, ref, blanket, variadic) — which a flat positional
        // list cannot express. Method-level slots stay abstract: inference
        // solves them at the call site.
        let impl_slots: IndexMap<u32, TypeId> = scope
            .annotate_ctx
            .trait_ctx
            .type_params
            .values()
            .copied()
            .collect();
        // A binding naming a type private to the declaring module (`type Iter
        // = TreeSetIter<T>`) means what the block wrote, not what the caller's
        // perspective can see (issue #1416) — which is why the decl pass, not
        // this query, resolved it.
        let signatures = Rc::clone(&scope.tysys.signatures);
        let impl_sig = signatures
            .impl_sig(impl_ref.1)
            .expect("the decl pass records every impl block's declaration facts")
            .instantiate_slots(&scope.tysys.type_table, &impl_slots);
        scope.annotate_ctx.trait_ctx.assoc_type_bindings.extend(
            impl_sig
                .associated_types
                .iter()
                .map(|(n, &t)| (n.clone(), t)),
        );

        let blanket_type_param = if is_blanket_type_param {
            Some(impl_struct_name.clone())
        } else {
            None
        };

        // Detect blanket ref impls: `impl<T: Bound> Trait for &T` where the inner type
        // is a type parameter. These should NOT override base-type methods.
        let is_blanket_ref_impl = match &header.ty {
            Type::Reference(inner) | Type::MutReference(inner) => {
                if let Type::Named(named) = inner.as_ref() {
                    // Inner is a bare name — check if it's a type parameter
                    header.type_params.iter().any(|tp| tp.name == named.name)
                } else {
                    false
                }
            }
            _ => false,
        };

        // The method's canonical signature comes from the decl pass; only its
        // type parameters and the impl's trait reference come off the header.
        let method_data = header
            .methods
            .iter()
            .find(|m| m.name == method_name)
            .map(|m| {
                let sig = scope
                    .tysys
                    .signatures
                    .method_sig(m.ast_id)
                    .expect("the decl pass records every impl-declared method's signature")
                    .clone();
                (sig, m.type_params.clone())
            });
        let trait_type_for_name = header.trait_type.as_ref().unwrap().clone();
        let target_for_name = header.ty.clone();
        // The trait's identity, resolved in the impl's own frame: the decl key
        // from the impl module's imports (so an alias resolves to the declaring
        // module), the arguments with the impl's bound type params substituted
        // (so `impl<T> Take<T> for Wrapper<T>` on `Wrapper<i32>` reads as
        // `Take<i32>`). Spellings never carry identity (WEP 2026-07-31).
        // `check_impl_trait_resolves` has already rejected a header whose trait
        // reaches no declaration, and such a block implements no trait — so it
        // contributes no trait method here. The index still holds it, keyed on
        // the spelling it wrote, which is how an erroneous block reaches a
        // lookup at all; a candidate built without an identity keys on nothing.
        let Some(trait_decl) = signatures
            .impl_sig(impl_ref.1)
            .expect("the decl pass records every impl block's declaration facts")
            .trait_decl
        else {
            return found_traits;
        };
        let defs = scope.tysys.resolutions.defs().clone();
        let trait_args = impl_sig.trait_type_args;

        let mut method_found = false;
        if let Some((method_sig, method_type_params)) = method_data {
            let self_kind = method_sig.self_kind;

            // Bring the reported slots into scope, so the body resolves `T`
            // to the slot the call site binds. Effect and `fn`-bound params
            // occupy none, so they are not registered.
            let method_slot_params: Vec<&ast::GenericParam> = method_type_params
                .iter()
                .filter(|p| p.is_real_type_param())
                .collect();
            let method_type_param_ids: Vec<TypeId> = method_sig.own_type_param_ids();
            assert_eq!(
                method_slot_params.len(),
                method_type_param_ids.len(),
                "`{method_name}` declares {} slots, its signature reports {}",
                method_slot_params.len(),
                method_type_param_ids.len()
            );
            for (type_param, &type_param_id) in
                method_slot_params.iter().zip(method_type_param_ids.iter())
            {
                let index = match scope.tysys.type_table.borrow().get(type_param_id) {
                    ResolvedType::TypeParam { index, .. }
                    | ResolvedType::TypePack { index, .. } => *index,
                    other => panic!("method slot is not a type parameter: {other:?}"),
                };
                scope
                    .annotate_ctx
                    .trait_ctx
                    .type_params
                    .insert(type_param.name.clone(), (index, type_param_id));
                if !type_param.bounds.is_empty() {
                    scope
                        .annotate_ctx
                        .trait_ctx
                        .type_param_bounds
                        .insert(type_param.name.clone(), type_param.bounds.clone());
                }
            }

            // `Self` needs no special handling: the canonical frame bound it
            // to the impl target, so filling the slots with the receiver's
            // arguments yields the concrete receiver.
            let instantiated = method_sig
                .decl
                .instantiate_slots(&scope.tysys.type_table, &impl_slots);
            let return_type = instantiated.return_type;
            // `MethodInfo::param_types` excludes the receiver; the digest
            // includes it.
            let param_types = instantiated.param_types[method_sig
                .first_value_param()
                .min(instantiated.param_types.len())..]
                .to_vec();

            // Remove method-level type params from scope
            for type_param in &method_type_params {
                scope
                    .annotate_ctx
                    .trait_ctx
                    .type_params
                    .shift_remove(&type_param.name);
                scope
                    .annotate_ctx
                    .trait_ctx
                    .type_param_bounds
                    .shift_remove(&type_param.name);
            }
            let param_is_mut = crate::elaborator::sig::Param::is_mut_flags(&method_sig.params);
            let param_names = crate::elaborator::sig::Param::names(&method_sig.params);
            // Parameter defaults live on the trait declaration only (WEP
            // 2026-04-11). Pull them from the trait's method, keyed by
            // parameter name, instead of the impl's re-specified params.
            let trait_name_base = scope.get_type_name(&trait_type_for_name);
            let param_defaults: Vec<Option<ast::Expr>> = {
                let trait_method_params = scope
                    .trait_sig_by_name(&trait_name_base)
                    .and_then(|sig| sig.method(method_name))
                    .map(|method| method.sig.params.clone());
                param_names
                    .iter()
                    .map(|name| {
                        trait_method_params.as_ref().and_then(|tp| {
                            tp.iter()
                                .find(|p| &p.name == name)
                                .and_then(|p| p.default.clone())
                        })
                    })
                    .collect()
            };
            found_traits.push(TraitMethodMatch {
                trait_name: scope.tysys.trait_env.fq_trait_named_by_impl(
                    crate::name::FqTraitName::declared(&defs, trait_decl).with_args(
                        super::trait_env::written_type_args(
                            &trait_type_for_name,
                            &scope.tysys.resolutions,
                        ),
                    ),
                    &trait_type_for_name,
                    &target_for_name,
                    &scope.tysys.resolutions,
                ),
                trait_decl,
                trait_args: trait_args.clone(),
                method_info: MethodInfo {
                    method_ast_id: Some(method_sig.ast_id),
                    return_type,
                    self_kind,
                    param_types,
                    param_is_mut,
                    owner: MethodOwner::Receiver,
                    cm_name: None,
                    is_ref_impl: false,
                    method_type_param_ids,
                    method_own_params: method_sig.own_params,
                    impl_module: Some(impl_module_source.clone()),
                    from_concrete_impl: impl_is_concrete,
                    param_defaults,
                    param_names,
                    consumes_self: self_kind == ast::SelfKind::Value,
                },
                impl_module_source: impl_module_source.clone(),
                blanket_type_param: blanket_type_param.clone(),
                impl_struct_name: impl_struct_name.clone(),
                impl_struct_fq: impl_struct_fq.clone(),
                is_blanket_ref_impl,
                is_variadic_impl: variadic_pack_entry.is_some(),
            });
            method_found = true;
        }

        // If the method wasn't found in the impl block, check the trait
        // declaration for a default method with that name
        if !method_found {
            let trait_name_base = scope.get_type_name(&trait_type_for_name);
            if let Some(default_method) = scope
                .trait_sig_by_name(&trait_name_base)
                .and_then(|sig| sig.method(method_name))
                .filter(|m| m.default_body.is_some())
                .cloned()
            {
                let mut declaring_args = vec![receiver_type_id.unwrap_or(TypeTable::UNKNOWN)];
                declaring_args.extend(trait_args.iter().copied());
                let instantiated = default_method.sig.instantiate_call(
                    &scope.tysys.type_table,
                    &declaring_args,
                    &[],
                );

                let self_kind = default_method.sig.self_kind;
                let first_value_param = default_method.sig.first_value_param();
                found_traits.push(TraitMethodMatch {
                    trait_name: scope.tysys.trait_env.fq_trait_named_by_impl(
                        crate::name::FqTraitName::declared(&defs, trait_decl).with_args(
                            super::trait_env::written_type_args(
                                &trait_type_for_name,
                                &scope.tysys.resolutions,
                            ),
                        ),
                        &trait_type_for_name,
                        &target_for_name,
                        &scope.tysys.resolutions,
                    ),
                    trait_decl,
                    trait_args: trait_args.clone(),
                    method_info: MethodInfo {
                        method_ast_id: Some(default_method.sig.ast_id),
                        return_type: instantiated.return_type,
                        self_kind,
                        param_types: instantiated.param_types[first_value_param..].to_vec(),
                        param_is_mut: crate::elaborator::sig::Param::is_mut_flags(
                            &default_method.sig.params,
                        ),
                        owner: MethodOwner::Receiver,
                        cm_name: None,
                        is_ref_impl: false,
                        method_type_param_ids: default_method.sig.own_type_param_ids(),
                        method_own_params: default_method.sig.own_params.clone(),
                        impl_module: Some(impl_module_source.clone()),
                        from_concrete_impl: impl_is_concrete,
                        param_defaults: default_method
                            .sig
                            .params
                            .iter()
                            .map(|p| p.default.clone())
                            .collect(),
                        param_names: crate::elaborator::sig::Param::names(
                            &default_method.sig.params,
                        ),
                        consumes_self: self_kind == ast::SelfKind::Value,
                    },
                    impl_module_source,
                    blanket_type_param,
                    impl_struct_name,
                    impl_struct_fq,
                    is_blanket_ref_impl,
                    is_variadic_impl: variadic_pack_entry.is_some(),
                });
            }
        }

        // Trait context is auto-restored on drop(scope).
        drop(scope);

        found_traits
    }

    /// Choose the winning match: drop a trait's variadic impl when that same
    /// trait also has a non-variadic one (coherence Rule 1, WEP 2026-03-14 §5),
    /// prefer a trait impl in the current module, dedup `(trait, module)`
    /// pairs, take the first remaining.
    fn select_trait_match(
        &mut self,
        mut found_traits: Vec<super::types::TraitMethodMatch>,
        method_name: &str,
        span: Span,
        probe: Option<&mut ArgProbe<'_>>,
    ) -> Option<super::types::TraitMethodMatch> {
        // Rule 1 ranks the impls of *one* trait against each other, so it must
        // not outrank locality between traits: a foreign blanket
        // `impl<T> A for T` would otherwise beat a local `impl<..T> B for [..T]`.
        let traits_with_non_variadic: IndexSet<(crate::defs::DefId, Vec<TypeId>)> = found_traits
            .iter()
            .filter(|m| !m.is_variadic_impl)
            .map(|m| (m.trait_decl, m.trait_args.clone()))
            .collect();
        found_traits.retain(|m| {
            !m.is_variadic_impl
                || !traits_with_non_variadic.contains(&(m.trait_decl, m.trait_args.clone()))
        });

        // Sort BEFORE dedup_by, since dedup_by only removes adjacent
        // duplicates. A blanket is the general case and loses to any impl
        // written for the receiver — even a local blanket to a foreign
        // concrete impl — so concrete-vs-blanket ranks above locality.
        let current_module = &self.current_module_source;
        found_traits.sort_by(|a, b| {
            let a_concrete = a.blanket_type_param.is_none();
            let b_concrete = b.blanket_type_param.is_none();
            let a_local = &a.impl_module_source == current_module;
            let b_local = &b.impl_module_source == current_module;
            b_concrete.cmp(&a_concrete).then(b_local.cmp(&a_local))
        });
        found_traits.dedup_by(|a, b| {
            a.trait_decl == b.trait_decl
                && a.trait_args == b.trait_args
                && a.impl_module_source == b.impl_module_source
        });

        // Distinct same-name declarations tie-break on scope: a same-named
        // foreign trait the calling module never imported is not a competitor
        // (`cross_module_same_name_foreign_impl.wado` — each module's
        // `s.shout()` dispatches to the `Loud` in scope there). Only when
        // several colliding declarations are in scope does the cross-trait
        // ambiguity below stand.
        let distinct: IndexSet<crate::defs::DefId> = found_traits
            .iter()
            .filter(|m| m.blanket_type_param.is_none())
            .map(|m| m.trait_decl)
            .collect();
        if distinct.len() > 1 {
            let visible: IndexSet<crate::defs::DefId> = distinct
                .iter()
                .filter(|d| self.trait_decl_in_scope(**d))
                .copied()
                .collect();
            if visible.len() == 1 {
                found_traits
                    .retain(|m| m.blanket_type_param.is_some() || visible.contains(&m.trait_decl));
            }
        }
        // WEP 2026-07-31: one trait declaration at several argument lists —
        // the arguments choose. The overload set is the concrete candidates of
        // one declaration; distinct traits never form an overload set, so a
        // cross-trait collision falls through to its error untouched, and
        // blanket candidates neither form nor defeat the set (they lose to any
        // concrete impl regardless).
        let concrete: Vec<usize> = found_traits
            .iter()
            .enumerate()
            .filter(|(_, m)| m.blanket_type_param.is_none())
            .map(|(i, _)| i)
            .collect();
        let mut classes: Vec<ArgClass> = Vec::new();
        if let Some(probe) = probe
            && concrete.len() > 1
            && concrete
                .iter()
                .all(|&i| found_traits[i].trait_decl == found_traits[concrete[0]].trait_decl)
            && concrete
                .iter()
                .any(|&i| found_traits[i].trait_args != found_traits[concrete[0]].trait_args)
        {
            // The overload set is what makes classification worth its cost, so
            // the arguments are synthesized here and nowhere earlier.
            classes = (0..probe.len()).map(|i| probe.class(self, i)).collect();
            // A candidate the call cannot fill at all is the arity error's to
            // report, not selection's, so it is not counted as a rejection.
            let applicable: Vec<usize> = concrete
                .iter()
                .copied()
                .filter(|&i| classes.len() <= found_traits[i].method_info.param_types.len())
                .collect();
            let admitted: Vec<usize> = applicable
                .iter()
                .copied()
                .filter(|&i| {
                    classes
                        .iter()
                        .zip(found_traits[i].method_info.param_types.iter())
                        .all(|(class, &param)| self.class_admits(param, class))
                })
                .collect();
            // Unique-or-error: exactly one admitted candidate wins. No ranking
            // — a literal admits every numeric width, so it never selects
            // between them. Several leave the set for the ambiguity report; none
            // is not ambiguity at all, and reports separately.
            match admitted.as_slice() {
                [winner] => {
                    let m = found_traits.swap_remove(*winner);
                    found_traits = vec![m];
                }
                [] if !applicable.is_empty() => {
                    self.report_no_admitted_overload(&found_traits, method_name, &classes, span);
                    return found_traits.into_iter().next();
                }
                [] | [_, _, ..] => {}
            }
        }

        self.report_trait_argument_ambiguity(&found_traits, method_name, &classes, span);
        self.report_cross_trait_ambiguity(&found_traits, method_name, span);
        // Still return a winner: reporting and then claiming the method is
        // missing would stack a second, wrong diagnostic on the same call.
        found_traits.into_iter().next()
    }

    /// One trait implemented for a receiver at two argument lists leaves the
    /// method name pointing at two signatures, and nothing downstream can choose:
    /// arguments are elaborated *against* the chosen signature, and Wado has no
    /// qualified call form to name one. Operators do not come through here —
    /// [`Self::find_indexing_trait_impl`] and friends pick by operand type, which
    /// is how `List<T>` carries two `IndexValue` impls. `classes` is what each
    /// argument contributed, so the message can name the one that gave selection
    /// nothing to work with.
    fn report_trait_argument_ambiguity(
        &self,
        found_traits: &[super::types::TraitMethodMatch],
        method_name: &str,
        classes: &[ArgClass],
        span: Span,
    ) {
        let Some(traits) = Self::overload_spellings(found_traits) else {
            return;
        };
        let _ = self.emit(TypeError::AmbiguousTraitArguments {
            method: method_name.to_string(),
            traits,
            arguments: self.describe_arg_classes(classes),
            span,
        });
    }

    /// The other end of the same rule: every argument list of the overload set
    /// rejected the arguments. A class only ever over-approximates, so no
    /// candidate admitting it means no candidate can accept it — and telling the
    /// caller to annotate an already-pinned argument would be wrong advice.
    fn report_no_admitted_overload(
        &self,
        found_traits: &[super::types::TraitMethodMatch],
        method_name: &str,
        classes: &[ArgClass],
        span: Span,
    ) {
        let Some(traits) = Self::overload_spellings(found_traits) else {
            return;
        };
        let _ = self.emit(TypeError::NoMatchingOverload {
            method: method_name.to_string(),
            traits,
            arguments: self.describe_arg_classes(classes),
            span,
        });
    }

    /// The competing spellings of one declaration's overload set, in candidate
    /// order, or `None` when the candidates are not an overload set.
    fn overload_spellings(found_traits: &[super::types::TraitMethodMatch]) -> Option<Vec<String>> {
        let first = found_traits.first()?;
        let rivals: Vec<&super::types::TraitMethodMatch> = found_traits
            .iter()
            .filter(|m| m.trait_decl == first.trait_decl && m.trait_args != first.trait_args)
            .collect();
        if rivals.is_empty() {
            return None;
        }
        let mut traits: Vec<String> = std::iter::once(first)
            .chain(rivals)
            .map(|m| m.trait_name.to_display())
            .collect();
        traits.dedup();
        Some(traits)
    }

    /// The reason chain both selection failures carry: what each argument
    /// contributed, in argument order.
    fn describe_arg_classes(&self, classes: &[ArgClass]) -> Vec<String> {
        classes
            .iter()
            .enumerate()
            .map(|(i, class)| format!("argument {} {}", i + 1, self.describe_arg_class(class)))
            .collect()
    }

    /// Two *different* traits declaring one method name for one receiver. The
    /// candidates share no contract, so there is nothing to select on; the
    /// call must name the trait (`Alpha::describe(&x)`). Reported where it is
    /// called, matching the bounds path's `AmbiguousTraitMethod` — the same
    /// rule, so the shape a collision arrives in does not change the answer
    /// (WEP 2026-07-31).
    fn report_cross_trait_ambiguity(
        &self,
        found_traits: &[super::types::TraitMethodMatch],
        method_name: &str,
        span: Span,
    ) {
        // Blanket candidates are excluded from the count: a blanket loses to
        // any impl written for the receiver, and counting it would make a
        // library adding one a breaking change for every downstream method of
        // that name (WEP 2026-07-31, blanket exception). Selection is
        // untouched — this decides only what is reported.
        // The collision is counted on declarations, so two same-named traits
        // from different modules still collide even though their spellings
        // agree — identity is the declaration, not the name.
        let mut seen: IndexSet<crate::defs::DefId> = IndexSet::default();
        for m in found_traits
            .iter()
            .filter(|m| m.blanket_type_param.is_none())
        {
            seen.insert(m.trait_decl);
        }
        if seen.len() < 2 {
            return;
        }
        // Same-named declarations are qualified by their declaring module —
        // a bare `'Kind' and 'Kind'` names nothing the user can act on.
        let defs = self.tysys.resolutions.defs();
        let mut bases: Vec<String> = seen
            .iter()
            .map(|d| {
                let name = defs.name(*d);
                if seen.iter().filter(|o| defs.name(**o) == name).count() > 1 {
                    format!("{name} (from \"{}\")", defs.module(*d))
                } else {
                    name.to_string()
                }
            })
            .collect();
        bases.sort();
        bases.dedup();
        let _ = self.emit(TypeError::AmbiguousTraitMethod {
            method: method_name.to_string(),
            traits: bases,
            span,
        });
    }

    /// Run an indexing-trait lookup on `(struct_name, base_type_id)`, falling
    /// back to the receiver's newtype base `(lookup_name, lookup_type_id)` only
    /// when it actually differs. For a non-newtype receiver the base equals the
    /// primary, so the former `lookup(primary).or_else(|| lookup(base))` re-ran
    /// the identical scan on every `a[i]`; this skips that redundant call.
    ///
    /// The matched receiver's `TypeId` comes back with the hit, so the caller
    /// names the dispatched method after the type whose impl actually matched.
    pub(super) fn index_lookup_or_newtype_base<T>(
        &mut self,
        struct_name: &str,
        base_type_id: TypeId,
        lookup_name: &str,
        lookup_type_id: TypeId,
        mut lookup: impl FnMut(&mut Self, &str, TypeId) -> Option<T>,
    ) -> Option<(T, TypeId)> {
        if let Some(found) = lookup(self, struct_name, base_type_id) {
            return Some((found, base_type_id));
        }
        if lookup_name != struct_name || lookup_type_id != base_type_id {
            return lookup(self, lookup_name, lookup_type_id).map(|found| (found, lookup_type_id));
        }
        None
    }

    /// The fq receiver name for an indexing-trait dispatch on `matched_type_id`
    /// — the `TypeId` [`Self::index_lookup_or_newtype_base`] reports alongside
    /// the impl it found.
    pub(super) fn fq_index_receiver(&self, matched_type_id: TypeId) -> crate::name::FqTypeName {
        self.tysys.fq_receiver_head(matched_type_id)
    }

    /// Find an `IndexRef` impl for a type. `expected_index_type` disambiguates
    /// overloaded impls (so a `Range` subscript does not match an `IndexRef<i32>`);
    /// `None` matches by container name alone.
    pub(super) fn find_index_trait_impl(
        &mut self,
        struct_name: &str,
        base_type_id: TypeId,
        expected_index_type: Option<TypeId>,
    ) -> Option<IndexTraitInfo> {
        self.find_indexing_trait_impl(
            struct_name,
            base_type_id,
            CompilerItem::IndexRef,
            "index_ref",
            "Output",
            expected_index_type,
        )
        .map(
            |(output_type, self_kind, trait_name, impl_module_source, index_type)| IndexTraitInfo {
                output_type,
                self_kind,
                trait_name,
                impl_module_source,
                index_type,
            },
        )
    }

    /// [`Self::find_index_trait_impl`] over `IndexRefMut`, for a `&mut` subscript.
    /// Its `Output: RefMut` bound is what declines a replace-on-assign element and
    /// sends the caller back to the shared lookup.
    pub(super) fn find_index_mut_trait_impl_as_ref(
        &mut self,
        struct_name: &str,
        base_type_id: TypeId,
        expected_index_type: Option<TypeId>,
    ) -> Option<IndexTraitInfo> {
        self.find_indexing_trait_impl(
            struct_name,
            base_type_id,
            CompilerItem::IndexRefMut,
            "index_ref_mut",
            "Output",
            expected_index_type,
        )
        .map(
            |(output_type, self_kind, trait_name, impl_module_source, index_type)| IndexTraitInfo {
                output_type,
                self_kind,
                trait_name,
                impl_module_source,
                index_type,
            },
        )
    }

    /// Find `KeyValueLiteralBuilder` trait implementation for a type.
    ///
    /// Checks first for an explicit `impl KeyValueLiteralBuilder for T` (with `Output = T`
    /// check for blanket-style self-as-builder usage), then falls back to checking whether
    /// `T` implements the `KeyValueLiteral` trait (separate builder pattern).
    pub(super) fn find_key_value_literal_trait_impl(
        &mut self,
        struct_name: &str,
        base_type_id: TypeId,
    ) -> Option<KeyValueLiteralTraitInfo> {
        // Primary: explicit impl KeyValueLiteralBuilder for T (self-as-builder pattern)
        if let Some((value_type, self_kind, trait_name, _, _)) = self.find_indexing_trait_impl(
            struct_name,
            base_type_id,
            CompilerItem::KeyValueLiteralBuilder,
            "insert_literal",
            "Value",
            None,
        ) {
            // Check if Output = Self (self-as-builder pattern)
            let output_type = self
                .find_assoc_type_in_trait_impl(
                    struct_name,
                    base_type_id,
                    CompilerItem::KeyValueLiteralBuilder,
                    "Output",
                )
                .unwrap_or(TypeTable::UNKNOWN);
            // Accept if no Output constraint mismatch (output == Self or unknown)
            if output_type == TypeTable::UNKNOWN || output_type == base_type_id {
                return Some(KeyValueLiteralTraitInfo {
                    value_type,
                    builder_type: base_type_id,
                    self_kind,
                    trait_name,
                });
            }
        }

        // Secondary: explicit impl KeyValueLiteral for T with type Builder (separate builder
        // pattern for immutable output types).
        let builder_type = self.find_assoc_type_in_trait_impl(
            struct_name,
            base_type_id,
            CompilerItem::KeyValueLiteral,
            "Builder",
        )?;
        let builder_name = self.tysys.struct_name_for_type(builder_type)?;
        if let Some((value_type, self_kind, trait_name, _, _)) = self.find_indexing_trait_impl(
            &builder_name,
            builder_type,
            CompilerItem::KeyValueLiteralBuilder,
            "insert_literal",
            "Value",
            None,
        ) {
            return Some(KeyValueLiteralTraitInfo {
                value_type,
                builder_type,
                self_kind,
                trait_name,
            });
        }

        None
    }

    /// Find the value of a specific associated type in a trait impl for a given struct.
    fn find_assoc_type_in_trait_impl(
        &mut self,
        struct_name: &str,
        base_type_id: TypeId,
        item: CompilerItem,
        assoc_name: &str,
    ) -> Option<TypeId> {
        let concrete_type_args = self
            .tysys
            .type_table
            .borrow()
            .nominal_type_args(base_type_id)
            .unwrap_or_default();

        let trait_ = self.tysys.compiler_trait_def(item)?;
        self.probe_trait_impls(
            &self.impl_target_of(base_type_id, &crate::name::DeclName::new(struct_name)),
            &concrete_type_args,
            |_, found| found == Some(trait_),
            |s, impl_ref, impl_sig, declared| {
                let trait_env = Arc::clone(&s.tysys.trait_env);
                let header = impl_header(&trait_env, impl_ref);
                let binding = *impl_sig.associated_types.get(assoc_name)?;
                if !s.tysys.verify_impl_type_compatibility(
                    &header.ty,
                    &concrete_type_args,
                    declared,
                ) {
                    return None;
                }
                Some(binding)
            },
        )
    }

    /// Find `SequenceLiteralBuilder` trait implementation for a type.
    ///
    /// Checks for an explicit `impl SequenceLiteralBuilder for T` (self-as-builder) first.
    /// If not found, checks for `impl SequenceLiteral for T` with `type Builder` (separate
    /// builder pattern for immutable output types).
    pub(super) fn find_sequence_literal_trait_impl(
        &mut self,
        struct_name: &str,
        base_type_id: TypeId,
    ) -> Option<SequenceLiteralTraitInfo> {
        // Primary: self-as-builder (impl SequenceLiteralBuilder for T)
        if let Some((element_type, self_kind, trait_name, impl_source, _)) = self
            .find_indexing_trait_impl(
                struct_name,
                base_type_id,
                CompilerItem::SequenceLiteralBuilder,
                "push_literal",
                "Element",
                None,
            )
        {
            let output_type = self
                .find_assoc_type_in_trait_impl(
                    struct_name,
                    base_type_id,
                    CompilerItem::SequenceLiteralBuilder,
                    "Output",
                )
                .unwrap_or(base_type_id);
            return Some(SequenceLiteralTraitInfo {
                element_type,
                builder_type: base_type_id,
                output_type,
                self_kind,
                trait_name,
                impl_module_source: impl_source,
            });
        }

        // Secondary: separate builder (impl SequenceLiteral for T with type Builder)
        let builder_type = self.find_assoc_type_in_trait_impl(
            struct_name,
            base_type_id,
            CompilerItem::SequenceLiteral,
            "Builder",
        )?;
        let builder_name = self.tysys.struct_name_for_type(builder_type)?;
        if let Some((element_type, self_kind, trait_name, impl_source, _)) = self
            .find_indexing_trait_impl(
                &builder_name,
                builder_type,
                CompilerItem::SequenceLiteralBuilder,
                "push_literal",
                "Element",
                None,
            )
        {
            let output_type = self
                .find_assoc_type_in_trait_impl(
                    &builder_name,
                    builder_type,
                    CompilerItem::SequenceLiteralBuilder,
                    "Output",
                )
                .unwrap_or(base_type_id);
            return Some(SequenceLiteralTraitInfo {
                element_type,
                builder_type,
                output_type,
                self_kind,
                trait_name,
                impl_module_source: impl_source,
            });
        }

        None
    }

    pub(super) fn find_index_assign_trait_impl(
        &mut self,
        struct_name: &str,
        base_type_id: TypeId,
    ) -> Option<IndexAssignTraitInfo> {
        self.find_indexing_trait_impl(
            struct_name,
            base_type_id,
            CompilerItem::IndexAssign,
            "index_assign",
            "Output",
            None,
        )
        .map(
            |(input_type, self_kind, trait_name, impl_module_source, index_type)| {
                IndexAssignTraitInfo {
                    input_type,
                    self_kind,
                    trait_name,
                    impl_module_source,
                    index_type,
                }
            },
        )
    }

    pub(super) fn find_index_mut_trait_impl(
        &mut self,
        struct_name: &str,
        base_type_id: TypeId,
    ) -> Option<IndexMutTraitInfo> {
        self.find_indexing_trait_impl(
            struct_name,
            base_type_id,
            CompilerItem::IndexRefMut,
            "index_ref_mut",
            "Output",
            None,
        )
        .map(
            |(output_type, self_kind, trait_name, impl_module_source, index_type)| {
                IndexMutTraitInfo {
                    output_type,
                    self_kind,
                    trait_name,
                    impl_module_source,
                    index_type,
                }
            },
        )
    }

    /// Find an `IndexValue` impl for a type. `expected_index_type` disambiguates
    /// overloaded impls; `None` matches by container name alone.
    pub(super) fn find_index_value_trait_impl(
        &mut self,
        struct_name: &str,
        base_type_id: TypeId,
        expected_index_type: Option<TypeId>,
    ) -> Option<IndexValueTraitInfo> {
        self.find_indexing_trait_impl(
            struct_name,
            base_type_id,
            CompilerItem::IndexValue,
            "index_value",
            "Output",
            expected_index_type,
        )
        .map(
            |(output_type, self_kind, trait_name, impl_module_source, index_type)| {
                IndexValueTraitInfo {
                    output_type,
                    self_kind,
                    trait_name,
                    impl_module_source,
                    index_type,
                }
            },
        )
    }

    /// Find operator trait implementation
    pub(super) fn find_arithmetic_trait_impl(
        &mut self,
        struct_name: &str,
        base_type_id: TypeId,
        trait_: crate::defs::DefId,
        method_name: &str,
        rhs: Option<&ArgClass>,
    ) -> Option<ArithmeticTraitInfo> {
        match self
            .find_arithmetic_trait_impls(struct_name, base_type_id, trait_, method_name, rhs)
            .as_slice()
        {
            [only] => Some(only.clone()),
            // Unique-or-error, as everywhere else: several admitted impls are
            // the caller's to report, with the span it holds.
            [] | [_, _, ..] => None,
        }
    }

    /// Every impl of `trait_name` on the receiver whose right-hand parameter
    /// admits `rhs` — the operator counterpart of argument-directed selection
    /// (WEP 2026-07-31). `rhs` is a *class*, not a type: at the point the
    /// coercion lookup runs, the right operand may still be a literal that has
    /// not been given a type yet, and a literal admits without selecting.
    pub(super) fn find_arithmetic_trait_impls(
        &mut self,
        struct_name: &str,
        base_type_id: TypeId,
        trait_: crate::defs::DefId,
        method_name: &str,
        rhs: Option<&ArgClass>,
    ) -> Vec<ArithmeticTraitInfo> {
        // Get concrete type arguments from the base type (for generic instances)
        let concrete_type_args: Vec<TypeId> =
            if let ResolvedType::GenericInstance { type_args, .. } =
                self.tysys.type_table.borrow().get(base_type_id).clone()
            {
                type_args
            } else {
                Vec::new()
            };

        self.collect_trait_impls(
            &self.impl_target_of(base_type_id, &crate::name::DeclName::new(struct_name)),
            &concrete_type_args,
            |_, found| found == Some(trait_),
            |s, impl_ref, impl_sig, _declared| {
                // Check trait bounds on type parameters (e.g., impl<T: Eq> Eq for List<T>).
                // Shared with `lookup_method_info_uncached` and
                // `find_trait_impl_for_type_with_args`, so a bound-checking
                // fix for any AST shape applies to every caller.
                let trait_env = Arc::clone(&s.tysys.trait_env);
                let header = impl_header(&trait_env, impl_ref);
                if !s.tysys.check_impl_block_bounds(
                    &s.annotate_ctx,
                    &s.type_lookup(),
                    &header.type_params,
                    &header.ty,
                    Some(&concrete_type_args),
                ) {
                    return None;
                }

                // The method's signature comes from the decl pass, resolved
                // in the impl's frame; instantiating it with the receiver's
                // type arguments is what the by-name re-resolution below used
                // to approximate.
                let method_header = header.methods.iter().find(|m| m.name == method_name)?;
                let method_sig = s.tysys.signatures.method_sig(method_header.ast_id)?;
                let self_kind = method_sig.self_kind;
                let rhs_index = usize::from(self_kind != ast::SelfKind::None);
                let rhs_type = method_sig
                    .decl
                    .instantiate(&s.tysys.type_table, &concrete_type_args)
                    .param_types
                    .get(rhs_index)
                    .copied();

                // The declared parameter is `&Rhs`; the operand is the
                // referent, so admissibility compares against that.
                if let Some(class) = rhs {
                    let declared = rhs_type.map(|t| match s.tysys.type_table.borrow().get(t) {
                        ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => *inner,
                        _ => t,
                    });
                    if let Some(declared) = declared
                        && !s.class_admits(declared, class)
                    {
                        return None;
                    }
                }

                let output_type = impl_sig
                    .associated_types
                    .get("Output")
                    .copied()
                    .unwrap_or(base_type_id);

                Some(ArithmeticTraitInfo {
                    output_type,
                    self_kind,
                    // The *full* spelling (`Add<Feet>`), not the operator's
                    // base name: it is what the mangled method name
                    // discriminates instantiations on, exactly as the indexing
                    // path records `IndexValue<i32>`.
                    trait_name: s
                        .tysys
                        .trait_env
                        .fq_trait_of_impl(header, &s.tysys.resolutions)?,
                    rhs_type,
                })
            },
        )
    }

    /// Look up the type parameters of a static method from its impl header.
    /// Scans the pre-digested impl headers for `impl StructName { fn method_name<...> }`;
    /// `impl_headers` already covers every loaded module (including the current one).
    pub(super) fn lookup_static_method_type_params(
        &self,
        struct_name: &str,
        method_name: &str,
    ) -> Vec<ast::GenericParam> {
        for header in self.tysys.trait_env.impl_headers.values() {
            if super::trait_env::get_type_name_static(&header.ty) == struct_name {
                for method in &header.methods {
                    if method.name == method_name && !method.type_params.is_empty() {
                        return method.type_params.clone();
                    }
                }
            }
        }
        vec![]
    }

    /// Look up the type parameters of a function from its AST definition.
    pub(super) fn lookup_function_type_params(
        &self,
        callee: &super::callee::CalleeRef,
    ) -> Vec<ast::GenericParam> {
        let callee_module = &callee.module;
        let func_name = callee.name.as_str();
        let fn_type_params = &self.tysys.trait_env.function_type_params;
        // Entry-point callees are looked up in the current module's functions first.
        if callee_module.is_entry_point()
            && let Some(tps) =
                fn_type_params.get(&(self.current_module_source.clone(), func_name.to_string()))
        {
            return tps.clone();
        }
        if let Some(tps) = fn_type_params.get(&(callee_module.clone(), func_name.to_string())) {
            return tps.clone();
        }
        Vec::new()
    }

    /// Helper to find indexing trait implementations (Index, `IndexMut`, or `IndexAssign`)
    pub(super) fn find_indexing_trait_impl(
        &mut self,
        struct_name: &str,
        base_type_id: TypeId,
        item: CompilerItem,
        method_name: &str,
        assoc_type_name: &str,
        expected_index_type: Option<TypeId>,
    ) -> Option<(
        TypeId,
        ast::SelfKind,
        crate::name::FqTraitName,
        ModuleSource,
        Option<TypeId>,
    )> {
        let concrete_type_args = self
            .tysys
            .type_table
            .borrow()
            .nominal_type_args(base_type_id)
            .unwrap_or_default();

        let trait_ = self.tysys.compiler_trait_def(item)?;
        self.probe_trait_impls(
            &self.impl_target_of(base_type_id, &crate::name::DeclName::new(struct_name)),
            &concrete_type_args,
            |_, found| found == Some(trait_),
            |s, impl_ref, impl_sig, declared| {
                // The trait's index-type argument (`List<i32>` in `impl
                // Index<List<i32>>`), returned for subscript coercion and used
                // to disambiguate overlapping impls when `expected_index_type`
                // is set.
                let trait_env = Arc::clone(&s.tysys.trait_env);
                let header = impl_header(&trait_env, impl_ref);
                let index_type = impl_sig.trait_type_args.first().copied();
                if let Some(expected_idx_type) = expected_index_type
                    && let Some(resolved_trait_idx) = index_type
                    && resolved_trait_idx != expected_idx_type
                {
                    return None;
                }

                if !s.tysys.verify_impl_type_compatibility(
                    &header.ty,
                    &concrete_type_args,
                    declared,
                ) {
                    return None;
                }
                let impl_type_params = header.type_params.clone();
                let impl_ty = header.ty.clone();
                if !concrete_type_args.is_empty()
                    && !s.tysys.check_impl_block_bounds(
                        &s.annotate_ctx,
                        &s.type_lookup(),
                        &impl_type_params,
                        &impl_ty,
                        Some(&concrete_type_args),
                    )
                {
                    return None;
                }

                let trait_name = s
                    .tysys
                    .trait_env
                    .fq_trait_of_impl(header, &s.tysys.resolutions)?;
                // Find the method. Only its receiver shape is needed here —
                // the indexing types come from the impl's associated-type
                // bindings.
                let method_header = header.methods.iter().find(|m| m.name == method_name)?;
                let self_kind = s
                    .tysys
                    .signatures
                    .method_sig(method_header.ast_id)?
                    .self_kind;
                let impl_source = s.impl_block_module_source(impl_ref);

                let assoc_type = impl_sig
                    .associated_types
                    .get(assoc_type_name)
                    .copied()
                    .unwrap_or(TypeTable::UNKNOWN);

                Some((assoc_type, self_kind, trait_name, impl_source, index_type))
            },
        )
    }

    /// Try to resolve a method call on an index expression using `IndexMut`.
    /// Returns Some(TirExpr) if the method needs &mut self and the type implements `IndexMut`.
    /// Returns None if we should fall back to normal resolution (using Index).
    pub(super) fn try_resolve_index_mut_method_call(
        &mut self,
        index_expr: &ast::IndexExpr,
        method_call: &ast::MethodCallExpr,
        ctx: &mut FunctionContext,
    ) -> Option<TirExpr> {
        // First, resolve the indexed container to get its type
        let container_type = self.resolve_expr(&index_expr.expr, ctx, None);

        let base_type_id = match self.tysys.type_table.borrow().get(container_type) {
            ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => *inner,
            _ => container_type,
        };

        let struct_name = match self.tysys.type_table.borrow().get(base_type_id).clone() {
            ResolvedType::Struct { .. } | ResolvedType::GenericInstance { .. } => self
                .tysys
                .type_table
                .borrow()
                .nominal_head(base_type_id)
                .map(|(n, _)| n)?,
            _ => return None, // Not a struct type
        };

        let index_type = self.resolve_expr(&index_expr.index, ctx, None);

        let index_mut_info = self.find_index_mut_trait_impl(&struct_name, base_type_id)?;

        if let Some(key_type) = index_mut_info.index_type
            && key_type != index_type
        {
            let _ = self.resolve_expr(&index_expr.index, ctx, Some(key_type));
        }

        // First, look up method info on the OUTPUT type (what IndexMut returns)
        let output_type = index_mut_info.output_type;
        let output_base_type_id = match self.tysys.type_table.borrow().get(output_type) {
            ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => *inner,
            _ => output_type,
        };

        let (output_struct_name, output_module_source, output_type_args) = match self
            .tysys
            .type_table
            .borrow()
            .get(output_base_type_id)
            .clone()
        {
            ResolvedType::Struct { .. } => {
                let (n, m) = self
                    .tysys
                    .type_table
                    .borrow()
                    .nominal_head(output_base_type_id)
                    .expect("a struct names a declaration");
                (n, m, None)
            }
            ResolvedType::GenericInstance { type_args, .. } => (
                self.tysys
                    .type_table
                    .borrow()
                    .nominal_head(output_base_type_id)
                    .expect("a generic instance names a declaration")
                    .0,
                self.tysys
                    .type_table
                    .borrow()
                    .nominal_head(output_base_type_id)
                    .expect("a generic instance names a declaration")
                    .1,
                if type_args.is_empty() {
                    None
                } else {
                    Some(type_args)
                },
            ),
            _ => (
                self.tysys
                    .type_table
                    .borrow()
                    .mangle_type_name(output_base_type_id),
                self.current_module_source.clone(),
                None,
            ),
        };

        // Look up method info to check if it needs &mut self
        let mut method_info = self.lookup_method_info(output_type, &method_call.method);
        let mut method_trait_name: Option<crate::name::FqTraitName> = None;
        let mut method_trait_impl_source: Option<ModuleSource> = None;

        // This lookup commits the call's resolution (the desugared call is
        // built from it), so argument-directed selection applies here exactly
        // as on the plain method-call path.
        let mut probe = ArgProbe::new(&method_call.args, ctx);
        if method_info.is_none()
            && let Some(trait_match) = self.find_trait_method_for_type(
                &self.impl_target(&output_struct_name),
                &method_call.method,
                output_type_args.as_deref(),
                Some(output_type),
                method_call.span,
                None,
                Some(&mut probe),
            )
        {
            method_trait_name = Some(trait_match.trait_name);
            method_info = Some(trait_match.method_info);
            method_trait_impl_source = Some(trait_match.impl_module_source);
        }

        let MethodInfo {
            method_ast_id: _,
            return_type,
            self_kind,
            param_types,
            param_is_mut: method_param_is_mut,
            owner: _,
            cm_name: _,
            method_own_params: _,
            is_ref_impl: method_is_ref_impl,
            method_type_param_ids: _,
            impl_module: _,
            from_concrete_impl: _,
            param_defaults: method_param_defaults,
            param_names: method_param_names,
            consumes_self: _,
        } = method_info?;

        // Only use IndexMut if the method requires &mut self
        if self_kind != ast::SelfKind::MutRef {
            return None; // Method doesn't need &mut, fall back to Index
        }

        let container_fq = self.tysys.fq_receiver_head(base_type_id);
        let mangled_index_mut_name = MethodName::format_local(
            &container_fq,
            Some(&index_mut_info.trait_name),
            "index_ref_mut",
        );

        // IndexMut returns &mut Output
        let mut_ref_output_type = self
            .tysys
            .type_table
            .borrow_mut()
            .make_mut_ref(index_mut_info.output_type);

        // Inner-dispatch recording: keyed by the
        // `IndexExpr`'s `AstId`, capture the IndexMut::index_mut
        // dispatch decision so reify can reproduce the same
        // `*expr.index_mut(idx)` shape. The outer method's
        // `method_dispatch` entry (recorded below at
        // `record_method_dispatch`) tells reify the outer call;
        // this entry tells it the inner call.
        self.record_operator_dispatch(
            index_expr.id,
            super::sem::types::OperatorDispatch {
                function_ref: FunctionRef {
                    module_source: index_mut_info.impl_module_source.clone(),
                    name: mangled_index_mut_name,
                    monomorph_info: None,
                    method_info: Some(LocalMethodName::new(
                        container_fq,
                        Some(index_mut_info.trait_name.clone()),
                        "index_ref_mut".to_string(),
                    )),
                },
                self_kind: index_mut_info.self_kind,
                arg_ref_wraps: vec![false],
                return_type: mut_ref_output_type,
                // IndexMut dispatch is consumed by the IndexMut-method-call
                // path, which applies its own deref; the index-expr reify
                // arm is not used for it.
                needs_deref: false,
            },
        );

        // Reify (`reify_index_mut_method_call`) rebuilds the
        // inner `*expr.index_mut(idx)` from the recorded `operator_dispatch`
        // above; the combined walk only needed the dispatch fact. The
        // index was resolved above for its side effects.

        for (i, a) in method_call.args.iter().enumerate() {
            let expected = param_types.get(i).copied();
            self.resolve_expr(a, ctx, expected);
        }

        let type_args: Vec<TypeId> = method_call
            .type_args
            .iter()
            .map(|ty| self.resolve_type(ty))
            .collect();

        let output_fq = self.tysys.fq_receiver_head(output_base_type_id);
        let mangled_method_name =
            MethodName::format_local(&output_fq, method_trait_name.as_ref(), &method_call.method);

        // `module_source` is the body's home module: trait-impl block for
        // trait methods, otherwise the output type's defining module
        // (inherent methods live alongside the type).
        let method_call_module_source =
            method_trait_impl_source.unwrap_or_else(|| output_module_source.clone());

        let func = FunctionRef {
            module_source: method_call_module_source,
            name: mangled_method_name,
            monomorph_info: None,
            method_info: Some(LocalMethodName::new(
                output_fq,
                method_trait_name,
                method_call.method.clone(),
            )),
        };

        // The IndexMut rewrite is the only path building user-visible method-call
        // TIR without going through `resolve_method_call_with`, so record
        // dispatch here and let `m["k"].push(1)` leave the same annotation as an
        // ordinary call. The `AstId` is also tagged
        // `DesugarKind::IndexMutMethodCall`, so reify takes the expansion path.
        self.record_method_dispatch(
            Some(method_call.id),
            &func,
            self_kind,
            method_is_ref_impl,
            method_param_is_mut,
            method_param_names,
            method_param_defaults,
            return_type,
            type_args,
            false,
        );
        self.record_desugar(
            method_call.id,
            super::sem::types::DesugarKind::IndexMutMethodCall,
        );

        // Reify (`reify_index_mut_method_call`) rebuilds the
        // outer method call (and the `__index_mut_val` synthesis) from the
        // recorded `method_dispatch` + `IndexMutMethodCall` desugar; the
        // combined walk projects only the result type. The args were resolved
        // above for their fact-recording side effects.
        Some(placeholder(return_type, method_call.span))
    }

    /// The sole elaborator-side constructor of a method call — a
    /// [`TirExprKind::Call`] whose receiver heads its `args`. Centralising it
    /// gives one audit point for "every elaborator-emitted method call was
    /// typechecked against the callee's declared parameter types", though the
    /// typechecking itself stays the caller's job.
    pub(super) fn build_tir_method_call(
        receiver: TirExpr,
        func: FunctionRef,
        type_args: Vec<TypeId>,
        args: Vec<CallArg>,
        return_type: TypeId,
        span: crate::token::Span,
    ) -> TirExpr {
        TirExpr::new(
            TirExprKind::method_call(Box::new(receiver), func, type_args, args),
            return_type,
            span,
        )
    }
}

/// How the frame sees `name`: `Some(is_mut)` for a binding it can vouch for,
/// `None` when none claims the name and it may be a global.
///
/// A closure body's own scopes hold only its parameters and locals — the
/// enclosing frame's bindings reach it as captures, which the scopes alone
/// would leave unaccounted for.
fn binding_mutability(name: &str, ctx: &FunctionContext) -> Option<bool> {
    if let Some(local) = ctx.lookup(name) {
        return Some(local.is_mut);
    }
    // Reading through a `&mut` box is what a mutable capture is.
    if ctx.deref_overrides.contains_key(name) || ctx.outer_box_types.contains_key(name) {
        return Some(true);
    }
    ctx.outer_locals.get(name).map(|outer| outer.is_mut)
}
