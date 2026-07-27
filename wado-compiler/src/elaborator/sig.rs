//! Canonical declaration signatures and the one way to instantiate them.

use std::cell::RefCell;
use std::rc::Rc;

use crate::ast::AstId;
use crate::hashmap::IndexMap;
use crate::module_source::ModuleSource;
use crate::tir::{TirEffectOp, TypeId, TypeTable};

use super::sem::decls::FunctionSig;

/// Program-wide declaration facts, resolved once by the decl pass and
/// read-only afterwards (WEP 2026-07-10).
///
/// # Membership rule
///
/// One entry per source declaration, holding what that declaration *says* —
/// its signature, or the declaration-level datum it is. Nothing computed
/// from a use site, and nothing a later phase recomputes: after
/// `annotate_decls` no phase re-resolves a declaration signature from AST.
///
/// AST survives inside an entry only where the value is irreducibly AST and
/// the consumer is the walker or reify, never a query: parameter defaults
/// (re-resolved per call site under the callee's scope, WEP 2026-04-11),
/// associated-const value expressions, `__DATA__` contents.
///
/// Everything here is assembled from the per-module
/// [`super::sem::decls::ModuleDecls`] digests once every module's decl pass
/// has run, so a body walk reads any module's declarations — its own
/// included — without reaching for an AST.
#[derive(Default)]
pub(crate) struct Signatures {
    /// Canonical free-function signatures, declaring module → name.
    pub(crate) function_sigs: IndexMap<ModuleSource, Rc<IndexMap<String, FunctionSig>>>,

    /// Canonical method signatures, keyed by the method's globally-unique
    /// `AstId` — `impl`-block methods and `interface` / `resource`
    /// operations alike. `TraitEnv::impl_headers` carries each impl
    /// method's `ast_id` and [`Self::resource_method_ids`] names the
    /// operations, so a dispatch query goes index → signature without ever
    /// reaching for a declaration's AST.
    pub(crate) method_sigs: IndexMap<AstId, MethodSig>,

    /// `(declaration `AstId`, operation name)` → the operation's own
    /// `AstId`. The name-keyed index over [`Self::method_sigs`] for
    /// `interface` / `resource` operations, which callers reach by name
    /// rather than by node.
    pub(crate) resource_method_ids: IndexMap<(AstId, String), AstId>,

    /// Per-`impl`-block facts shared by the block's methods, keyed by the
    /// block's `AstId`.
    pub(crate) impl_sigs: IndexMap<AstId, ImplSig>,

    /// `interface` / `resource` operation signatures, keyed by the
    /// declaration's `AstId`.
    pub(crate) effect_ops: IndexMap<AstId, Vec<TirEffectOp>>,

    /// The same operations keyed by name for the lookups that have only a
    /// name: declaring module → `(decl name, op name)`.
    pub(crate) effect_op_sigs:
        IndexMap<ModuleSource, IndexMap<(String, String), (Vec<TypeId>, Option<TypeId>)>>,

    /// Global-variable declarations, declaring module → name →
    /// `(declared type, is_mut)`.
    pub(crate) globals: IndexMap<ModuleSource, IndexMap<String, (TypeId, bool)>>,

    /// Impl-associated constants, keyed by canonical identity
    /// `(type-declaring module, "Type::CONST")` → `(impl-declaring module,
    /// declared type, value expr)`. Canonical keys cannot collide across
    /// modules; consumers canonicalize the queried prefix the same way the
    /// decl pass did.
    pub(crate) associated_constants:
        IndexMap<(ModuleSource, String), (ModuleSource, TypeId, crate::ast::Expr)>,

    /// Per-module `__DATA__` section contents; modules without one have no
    /// entry.
    pub(crate) data_sections: IndexMap<ModuleSource, String>,
}

impl Signatures {
    /// Canonical signature of the free function `name` declared in `module`.
    pub(crate) fn function_sig(&self, module: &ModuleSource, name: &str) -> Option<&FunctionSig> {
        self.function_sigs.get(module)?.get(name)
    }

    /// Canonical signature of the method declared at `ast_id`.
    pub(crate) fn method_sig(&self, ast_id: AstId) -> Option<&MethodSig> {
        self.method_sigs.get(&ast_id)
    }

    /// Canonical signature of the operation `name` on the `interface` /
    /// `resource` declared at `decl_id`.
    pub(crate) fn resource_method_sig(&self, decl_id: AstId, name: &str) -> Option<&MethodSig> {
        let method_id = self.resource_method_ids.get(&(decl_id, name.to_string()))?;
        self.method_sig(*method_id)
    }

    /// Declaration facts of the `impl` block at `ast_id`.
    pub(crate) fn impl_sig(&self, ast_id: AstId) -> Option<&ImplSig> {
        self.impl_sigs.get(&ast_id)
    }

    /// Operation signatures of the `interface` / `resource` declared at
    /// `ast_id`, in any loaded module.
    pub(crate) fn effect_ops(&self, ast_id: AstId) -> Option<&[TirEffectOp]> {
        self.effect_ops.get(&ast_id).map(Vec::as_slice)
    }

    /// Signature of operation `op` on the `interface` / `resource` named
    /// `decl` in `module`.
    pub(crate) fn effect_op_sig(
        &self,
        module: &ModuleSource,
        decl: String,
        op: String,
    ) -> Option<&(Vec<TypeId>, Option<TypeId>)> {
        self.effect_op_sigs.get(module)?.get(&(decl, op))
    }

    /// Declared type and mutability of the global `name` in `module`.
    pub(crate) fn global(&self, module: &ModuleSource, name: &str) -> Option<(TypeId, bool)> {
        self.globals.get(module)?.get(name).copied()
    }

    /// The associated constant at canonical identity `(module, key)`.
    pub(crate) fn associated_constant(
        &self,
        module: &ModuleSource,
        key: String,
    ) -> Option<&(ModuleSource, TypeId, crate::ast::Expr)> {
        self.associated_constants.get(&(module.clone(), key))
    }

    /// The `__DATA__` section of `module`, if it declares one.
    pub(crate) fn data_section(&self, module: &ModuleSource) -> Option<&str> {
        self.data_sections.get(module).map(String::as_str)
    }
}

/// A declaration's parameter and return types, resolved once in its
/// declaring frame and abstract over the positional slots in
/// [`Self::type_params`] (WEP 2026-07-10).
///
/// Slot `i` is a `ResolvedType::TypeParam` (or `TypePack`) whose index is
/// `i`, so a use site's type arguments fill the slots positionally.
/// Effect parameters and `<F: fn(…)>` bounds consume no slot — they are
/// scope entries, not substitution targets — so this list is dense.
///
/// A use site that knows its type arguments reads the signature through
/// [`Self::instantiate`]. Inference, which solves *for* those arguments,
/// is the one consumer that reads the canonical types directly.
#[derive(Clone, Debug, Default)]
pub(crate) struct DeclSig {
    pub(crate) type_params: Vec<(String, TypeId)>,
    pub(crate) param_types: Vec<TypeId>,
    /// `None` when the declaration declares no return type.
    pub(crate) return_type: Option<TypeId>,
}

/// A method's canonical signature: the frame, plus what dispatch needs
/// about the method that is not a type. Those extras are not part of
/// [`DeclSig`] because nothing substitutes into them.
///
/// One record for both kinds of method declaration — an `impl` block's, in
/// the block's frame, and an `interface` / `resource` declaration's, in the
/// declaration's — because dispatch asks them the same questions.
#[derive(Clone, Debug)]
pub(crate) struct MethodSig {
    pub(crate) decl: DeclSig,
    pub(crate) self_kind: crate::ast::SelfKind,
    /// The non-receiver parameters, in order. `decl.param_types` includes
    /// the receiver at index 0 when there is one, so these are offset by
    /// [`Self::first_value_param`].
    pub(crate) params: Vec<Param>,
    /// Canonical name from `#[cm("…")]`, resolved at the declaration.
    pub(crate) cm_name: Option<String>,
    pub(crate) is_async: bool,
}

/// What a declaration says about one parameter beyond its type. One record
/// per parameter rather than a vector per attribute: callers read them
/// together, and parallel vectors can disagree in length or order.
#[derive(Clone, Debug)]
pub(crate) struct Param {
    pub(crate) name: String,
    pub(crate) is_mut: bool,
    /// Irreducibly AST — re-resolved per call site under the callee's scope
    /// (WEP 2026-04-11).
    pub(crate) default: Option<crate::ast::Expr>,
}

impl Param {
    pub(crate) fn names(params: &[Self]) -> Vec<String> {
        params.iter().map(|p| p.name.clone()).collect()
    }

    pub(crate) fn is_mut_flags(params: &[Self]) -> Vec<bool> {
        params.iter().map(|p| p.is_mut).collect()
    }

    pub(crate) fn named_defaults(params: &[Self]) -> Vec<(String, Option<crate::ast::Expr>)> {
        params
            .iter()
            .map(|p| (p.name.clone(), p.default.clone()))
            .collect()
    }
}

impl MethodSig {
    /// Index of the first non-receiver parameter in `decl.param_types`.
    pub(crate) fn first_value_param(&self) -> usize {
        usize::from(self.self_kind != crate::ast::SelfKind::None)
    }
}

/// One `impl` block's declaration facts, resolved once in the block's own
/// frame — its target type parameters occupying the positional slots the
/// block's methods are numbered against.
///
/// Not part of [`MethodSig`]: these are the *block's* facts, shared by every
/// method it declares, and a use site reads them without naming a method.
#[derive(Clone, Debug, Default)]
pub(crate) struct ImplSig {
    /// The impl target's type arguments (`K`, `V` in `impl … for Map<K, V>`).
    /// A slot appears as its own `TypeParam` / `TypePack`, so aligning a
    /// receiver's arguments against this list says which slot each fills.
    /// Empty when the target is not generic.
    pub(crate) target_type_args: Vec<TypeId>,
    /// The trait reference's type arguments (`K` in `impl Index<K> for …`),
    /// resolved against the same slots. Empty for an inherent impl.
    pub(crate) trait_type_args: Vec<TypeId>,
    /// The block's `type X = …;` bindings, resolved against the same slots.
    pub(crate) associated_types: IndexMap<String, TypeId>,
}

impl ImplSig {
    /// Fill the block's slots with a receiver's type arguments — the one way
    /// a use site reads an [`ImplSig`], mirroring [`DeclSig::instantiate`].
    pub(crate) fn instantiate(
        &self,
        type_table: &RefCell<TypeTable>,
        receiver_args: &[TypeId],
    ) -> InstantiatedImplSig {
        let slots = self.slots(type_table, receiver_args);
        let mut table = type_table.borrow_mut();
        InstantiatedImplSig {
            trait_type_args: self
                .trait_type_args
                .iter()
                .map(|&arg| table.substitute_type_params(arg, &slots))
                .collect(),
            associated_types: self
                .associated_types
                .iter()
                .map(|(name, &binding)| {
                    (name.clone(), table.substitute_type_params(binding, &slots))
                })
                .collect(),
        }
    }

    /// The slot substitution a receiver's type arguments imply.
    ///
    /// Target position `i` binds a slot only where the impl wrote a type
    /// parameter there; a concrete argument (`u8` in `impl List<u8>`) binds
    /// nothing, which is what makes a partially-concrete target expressible.
    fn slots(
        &self,
        type_table: &RefCell<TypeTable>,
        receiver_args: &[TypeId],
    ) -> IndexMap<u32, TypeId> {
        let table = type_table.borrow();
        self.target_type_args
            .iter()
            .zip(receiver_args)
            .filter_map(|(&declared, &concrete)| match table.get(declared) {
                crate::tir::ResolvedType::TypeParam { index, .. }
                | crate::tir::ResolvedType::TypePack { index, .. } => Some((*index, concrete)),
                _ => None,
            })
            .collect()
    }
}

/// An [`ImplSig`] with its slots filled by a receiver's type arguments.
#[derive(Clone, Debug)]
pub(crate) struct InstantiatedImplSig {
    pub(crate) trait_type_args: Vec<TypeId>,
    pub(crate) associated_types: IndexMap<String, TypeId>,
}

/// A [`DeclSig`] with its slots filled by a use site's type arguments.
#[derive(Clone, Debug)]
pub(crate) struct InstantiatedSig {
    pub(crate) param_types: Vec<TypeId>,
    pub(crate) return_type: TypeId,
}

impl DeclSig {
    /// Fill the signature's slots with `type_args`, positionally.
    ///
    /// Arity is the caller's contract: it owns the diagnostic for a
    /// mismatch, and passing fewer arguments than slots is how a
    /// partially-inferred call site instantiates what it knows, leaving
    /// the trailing slots abstract. Passing none substitutes nothing.
    pub(crate) fn instantiate(
        &self,
        type_table: &RefCell<TypeTable>,
        type_args: &[TypeId],
    ) -> InstantiatedSig {
        let substitution: IndexMap<u32, TypeId> = type_args
            .iter()
            .enumerate()
            .map(|(i, &t)| (i as u32, t))
            .collect();
        self.instantiate_slots(type_table, &substitution)
    }

    /// Fill slots by index rather than by position, for a caller that already
    /// holds a slot map. Equivalent to [`Self::instantiate`] when the map is
    /// dense from zero; the two differ only when the caller knows a
    /// non-contiguous subset, which positional arguments cannot express.
    pub(crate) fn instantiate_slots(
        &self,
        type_table: &RefCell<TypeTable>,
        substitution: &IndexMap<u32, TypeId>,
    ) -> InstantiatedSig {
        let mut table = type_table.borrow_mut();
        InstantiatedSig {
            param_types: self
                .param_types
                .iter()
                .map(|&p| table.substitute_type_params(p, substitution))
                .collect(),
            return_type: table
                .substitute_type_params(self.return_type.unwrap_or(TypeTable::UNIT), substitution),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `fn id<T>(x: T) -> T` instantiated at `T = i32`.
    fn generic_identity(table: &RefCell<TypeTable>) -> DeclSig {
        let t = table.borrow_mut().make_type_param("T".to_string(), 0);
        DeclSig {
            type_params: vec![("T".to_string(), t)],
            param_types: vec![t],
            return_type: Some(t),
        }
    }

    #[test]
    fn instantiate_fills_slots_positionally() {
        let table = RefCell::new(TypeTable::new());
        let sig = generic_identity(&table);

        let inst = sig.instantiate(&table, &[TypeTable::I32]);

        assert_eq!(inst.param_types, vec![TypeTable::I32]);
        assert_eq!(inst.return_type, TypeTable::I32);
    }

    #[test]
    fn instantiate_without_args_keeps_the_canonical_form() {
        let table = RefCell::new(TypeTable::new());
        let sig = generic_identity(&table);
        let canonical = sig.param_types.clone();

        let inst = sig.instantiate(&table, &[]);

        assert_eq!(inst.param_types, canonical);
        assert_eq!(inst.return_type, canonical[0]);
    }

    #[test]
    fn instantiate_substitutes_inside_containers() {
        let table = RefCell::new(TypeTable::new());
        let t = table.borrow_mut().make_type_param("T".to_string(), 0);
        let list_of_t = table.borrow_mut().make_ref(t);
        let sig = DeclSig {
            type_params: vec![("T".to_string(), t)],
            param_types: vec![list_of_t],
            return_type: None,
        };

        let inst = sig.instantiate(&table, &[TypeTable::I32]);

        let expected = table.borrow_mut().make_ref(TypeTable::I32);
        assert_eq!(inst.param_types, vec![expected]);
        assert_eq!(inst.return_type, TypeTable::UNIT);
    }

    #[test]
    fn a_partial_instantiation_leaves_trailing_slots_abstract() {
        let table = RefCell::new(TypeTable::new());
        let t = table.borrow_mut().make_type_param("T".to_string(), 0);
        let u = table.borrow_mut().make_type_param("U".to_string(), 1);
        let sig = DeclSig {
            type_params: vec![("T".to_string(), t), ("U".to_string(), u)],
            param_types: vec![t, u],
            return_type: Some(u),
        };

        let inst = sig.instantiate(&table, &[TypeTable::I32]);

        assert_eq!(inst.param_types, vec![TypeTable::I32, u]);
        assert_eq!(inst.return_type, u);
    }

    #[test]
    fn a_slotless_signature_is_unchanged_by_instantiation() {
        let table = RefCell::new(TypeTable::new());
        let sig = DeclSig {
            type_params: Vec::new(),
            param_types: vec![TypeTable::I32],
            return_type: Some(TypeTable::BOOL),
        };

        let inst = sig.instantiate(&table, &[]);

        assert_eq!(inst.param_types, vec![TypeTable::I32]);
        assert_eq!(inst.return_type, TypeTable::BOOL);
    }

    /// `impl<V> Index<i32> for Map<u8, V> { type Output = V; }` — the
    /// concrete `u8` position binds nothing, so `V` keeps its own slot.
    fn partially_concrete_impl(table: &RefCell<TypeTable>) -> ImplSig {
        let v = table.borrow_mut().make_type_param("V".to_string(), 1);
        ImplSig {
            target_type_args: vec![TypeTable::U8, v],
            trait_type_args: vec![TypeTable::I32],
            associated_types: [("Output".to_string(), v)].into_iter().collect(),
        }
    }

    #[test]
    fn only_type_parameter_positions_bind_slots() {
        let table = RefCell::new(TypeTable::new());
        let sig = partially_concrete_impl(&table);

        let slots = sig.slots(&table, &[TypeTable::U8, TypeTable::BOOL]);

        assert_eq!(slots.len(), 1);
        assert_eq!(slots.get(&1), Some(&TypeTable::BOOL));
    }

    #[test]
    fn an_associated_type_instantiates_through_its_slot() {
        let table = RefCell::new(TypeTable::new());
        let sig = partially_concrete_impl(&table);

        let inst = sig.instantiate(&table, &[TypeTable::U8, TypeTable::BOOL]);

        assert_eq!(inst.associated_types.get("Output"), Some(&TypeTable::BOOL));
        assert_eq!(inst.associated_types.get("Item"), None);
    }

    #[test]
    fn a_concrete_trait_argument_survives_instantiation() {
        let table = RefCell::new(TypeTable::new());
        let sig = partially_concrete_impl(&table);

        let inst = sig.instantiate(&table, &[TypeTable::U8, TypeTable::BOOL]);

        assert_eq!(inst.trait_type_args, vec![TypeTable::I32]);
    }
}
