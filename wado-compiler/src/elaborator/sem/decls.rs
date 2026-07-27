//! [`ModuleDecls`] — module-internal declarations confirmed by elaboration.
//!
//! Stage 3 of [`wep-2026-05-26-elaborator-rearchitecture.md`] populates the
//! fields below by relocating per-module state off
//! [`super::super::Elaborator`].
//!
//! # Membership rule
//!
//! Add a field here when it summarises a *declaration* in this module
//! after elaboration has resolved its signature: function return types,
//! generic parameter tables, global variable types, associated constants,
//! synthesised structures. If the fact is a *use-site* decision
//! (annotation on an [`crate::ast::AstId`]) it belongs in
//! [`super::types::TypeAnnotations`]; if it is an *import* fact derived
//! from a `use` declaration it belongs in [`super::imports::ModuleImports`].

use crate::ast;
use crate::hashmap::{IndexMap, IndexSet};
use crate::module_source::ModuleSource;
use crate::tir::TypeId;

use super::super::sig::{DeclSig, MethodSig};
use super::super::types::{EnumInfo, FlagsInfo, GenericNewtypeInfo, StructFieldInfo, VariantInfo};

/// A function's canonical signature, resolved once by its module's decl
/// pass in the declaring perspective, with the function's own type params
/// registered as `TypeParam` slots. Use sites substitute into this frame;
/// they never re-resolve the signature AST.
#[derive(Clone)]
pub(crate) struct FunctionSig {
    /// The canonical frame: the positional slots, the parameter types, and
    /// the return type. Instantiated at a use site through
    /// [`DeclSig::instantiate`].
    pub(crate) decl: DeclSig,
    /// Registered `(name, TypeId)` pairs, in registration order — real
    /// params as `TypeParam` slots, fn-bound params as their realised
    /// function type. Effect params are excluded; empty iff the function
    /// declares only effect params (or none). A superset of
    /// `decl.type_params`, which holds only the slot-consuming subset.
    pub(crate) type_param_ids: Vec<(String, TypeId)>,
    /// The declared parameters, in order, parallel to `decl.param_types`.
    pub(crate) params: Vec<super::super::sig::Param>,
    /// Declared `with` effects, resolved in the declaring perspective
    /// (effect parameters stay symbolic as `EffectRef::Param`).
    pub(crate) effects: Vec<crate::tir::EffectRef>,
}

impl ModuleDecls {
    /// Carry a snapshot module's decl digests into this compile. The digest
    /// field list lives only here; a digest missing from this method would
    /// silently lose stdlib facts on snapshot-hit builds only.
    pub(crate) fn clone_digests_from(&mut self, other: &ModuleDecls) {
        self.associated_constants
            .clone_from(&other.associated_constants);
        self.effect_ops.clone_from(&other.effect_ops);
        self.method_sigs.clone_from(&other.method_sigs);
        self.resource_method_ids
            .clone_from(&other.resource_method_ids);
        self.impl_sigs.clone_from(&other.impl_sigs);
        self.function_sigs = std::rc::Rc::clone(&other.function_sigs);
        self.current_module_globals
            .clone_from(&other.current_module_globals);
    }
}

/// Per-module declaration tables produced by elaboration.
#[derive(Default, Clone)]
pub(crate) struct ModuleDecls {
    /// Canonical signatures of this module's own free functions, frozen
    /// behind `Rc` so the program-wide assembly and the stdlib-snapshot
    /// seeding share the map instead of deep-cloning every signature.
    pub(crate) function_sigs: std::rc::Rc<IndexMap<String, FunctionSig>>,
    /// `func_name → return TypeId` for functions defined in this module.
    pub(crate) function_return_types: IndexMap<String, TypeId>,
    /// Names visible via `use` declarations in this module (the union of
    /// imported function names, effect function names, and namespace-import
    /// members), used by call resolution to recognise non-local names.
    pub(crate) imported_functions: IndexSet<String>,
    /// `name → (TypeId, is_mut)` for globals declared in this module.
    pub(crate) current_module_globals: IndexMap<String, (TypeId, bool)>,
    /// `local_name → (source, original_name, TypeId, is_mut)` for globals
    /// brought in by `use`.
    pub(crate) imported_globals: IndexMap<String, (ModuleSource, String, TypeId, bool)>,
    /// This module's own impl-associated constants, keyed by canonical
    /// identity `(type-declaring module, "Type::CONST")` — the impl
    /// target's prefix canonicalized in this module's scope. The value
    /// carries the impl-declaring module (reify walks the value expr under
    /// that perspective), the const type, and the value expression.
    /// Canonical keys make cross-module collisions impossible, so the
    /// driver-merged view needs no shadowing rules.
    pub(crate) associated_constants:
        IndexMap<(ModuleSource, String), (ModuleSource, TypeId, ast::Expr)>,
    /// Canonical signatures of this module's method declarations, keyed by
    /// the method's globally-unique `AstId`.
    ///
    /// An `impl` method is resolved in the impl's frame — impl type params
    /// in their positional slots, the method's own after them, `Self` bound
    /// to the impl target. An `interface` / `resource` operation is resolved
    /// in the declaration's frame. Either way a use site instantiates
    /// instead of re-resolving the method AST.
    pub(crate) method_sigs: IndexMap<crate::ast::AstId, MethodSig>,
    /// A declaration's `AstId` paired with an operation name → the
    /// `AstId`, so a caller holding only a name reaches its `method_sigs`
    /// entry.
    pub(crate) resource_method_ids: IndexMap<(crate::ast::AstId, String), crate::ast::AstId>,
    /// Facts of this module's `impl` blocks that belong to the block rather
    /// than to one method — its target and trait type arguments and its
    /// associated-type bindings — resolved once in the block's own frame and
    /// keyed by the block's `AstId`.
    pub(crate) impl_sigs: IndexMap<crate::ast::AstId, super::super::sig::ImplSig>,
    /// Resolved operation signatures of this module's `interface` and
    /// `resource` declarations, keyed by the declaration's `AstId`.
    ///
    /// Resolved in the declaration's own frame — type params registered and
    /// `Self` constructed — which is why the body pass reads these back
    /// instead of resolving the same methods a second time.
    pub(crate) effect_ops: IndexMap<crate::ast::AstId, Vec<crate::tir::TirEffectOp>>,

    /// Names of generic structs declared in this module (used to decide
    /// whether a struct reference needs generic-instance handling).
    pub(crate) generic_struct_names: IndexSet<String>,
    /// `func_name → type_params` for generic functions in this module.
    pub(crate) generic_function_params: IndexMap<String, Vec<(String, TypeId)>>,
    /// `func_name → resolved param TypeIds` for generic functions. The
    /// `TypeIds` are resolved in the function's own type-param scope so
    /// `TypeParam` ids match.
    pub(crate) generic_function_resolved_param_types: IndexMap<String, Vec<TypeId>>,
    /// `func_name → resolved return TypeId` for generic functions. Used for
    /// expected-return back-inference.
    pub(crate) generic_function_resolved_return_types: IndexMap<String, TypeId>,
    /// `mangled_name → type_params` for generic methods.
    pub(crate) generic_method_params: IndexMap<String, Vec<(String, TypeId)>>,
    /// `mangled_name → resolved param TypeIds` for generic methods. Resolved
    /// in the method's own type-param scope.
    pub(crate) generic_method_resolved_param_types: IndexMap<String, Vec<TypeId>>,

    /// Anonymous structs synthesised from struct literals during expression
    /// resolution. Flushed into the [`crate::tir::TirModule`] at the end of
    /// [`super::super::Elaborator::resolve_module`].
    pub(crate) pending_anonymous_structs: Vec<crate::tir::TirStruct>,

    /// Synthesis requests recorded by `impl Trait for Type;`
    /// (Gap 12 / Stage 5). The elaborator pushes one per
    /// `is_synthesize_request` impl block; `reify_module` reads
    /// this list and pushes each onto the emitted
    /// `TirModule::synthesis_requests`. Decouples annotate
    /// (which records) from reify (which emits).
    pub(crate) pending_synthesis_requests: Vec<crate::tir::SynthesisRequest>,

    /// Per-module additions to the type tables, consulted by
    /// [`super::super::types::TypeLookup`] before the shared `all_*` tables.
    /// Populated by [`super::super::Elaborator::collect_types`] and by call
    /// sites that intern an anonymous struct or instantiate a generic
    /// newtype on demand.
    pub(crate) local_struct_fields: IndexMap<String, StructFieldInfo>,
    pub(crate) local_newtypes: IndexMap<String, TypeId>,
    pub(crate) local_generic_newtypes: IndexMap<String, GenericNewtypeInfo>,
    pub(crate) local_enum_cases: IndexMap<String, EnumInfo>,
    pub(crate) local_flags_cases: IndexMap<String, FlagsInfo>,
    pub(crate) local_variant_cases: IndexMap<String, VariantInfo>,

    /// Function-local item declarations (`Stmt::Item` — a `struct`/`enum`/
    /// `variant`/`flags`/`type` declared inside a function body). Distinct
    /// from `local_struct_fields` et al above, which are module-scoped
    /// (anonymous struct literals, in-progress module decls): these are
    /// scoped to a single function, populated sequentially by
    /// `Elaborator::resolve_stmt` as it walks the body (no hoisting — a
    /// local item is visible only after its own declaration statement,
    /// matching `let`), and cleared at the start of every
    /// `Elaborator::resolve_function` call so sibling functions never see
    /// each other's local items. Consulted by `TypeLookup` with the highest
    /// precedence, ahead of `local_struct_fields` et al.
    pub(crate) fn_local_struct_fields: IndexMap<String, StructFieldInfo>,
    pub(crate) fn_local_newtypes: IndexMap<String, TypeId>,
    pub(crate) fn_local_enum_cases: IndexMap<String, EnumInfo>,
    pub(crate) fn_local_flags_cases: IndexMap<String, FlagsInfo>,
    pub(crate) fn_local_variant_cases: IndexMap<String, VariantInfo>,
}

impl ModuleDecls {
    /// Resolve a bare name to a module or imported global as
    /// `(source, original_name, type, is_mut)`. `source` is `current` for a
    /// locally-declared global. Both the annotate and reify call-resolution
    /// paths route their global-callee lookup through here so the two never
    /// disagree on what a bare name binds to.
    pub(crate) fn lookup_global(
        &self,
        name: &str,
        current: &ModuleSource,
    ) -> Option<(ModuleSource, String, TypeId, bool)> {
        self.current_module_globals
            .get(name)
            .map(|&(ty, mutable)| (current.clone(), name.to_string(), ty, mutable))
            .or_else(|| {
                self.imported_globals
                    .get(name)
                    .map(|(src, orig, ty, mutable)| (src.clone(), orig.clone(), *ty, *mutable))
            })
    }

    /// Clear the function-scoped local-item registries (`fn_local_*`).
    /// Called at the top of every function-body-walk entry point —
    /// `resolve_function`, `resolve_method`, `resolve_test_decl` — so a
    /// local item declared in one function body never leaks into the next
    /// one's resolution. A single shared call makes it impossible for a new
    /// entry point to forget this.
    pub(crate) fn clear_fn_local_items(&mut self) {
        self.fn_local_struct_fields.clear();
        self.fn_local_newtypes.clear();
        self.fn_local_enum_cases.clear();
        self.fn_local_flags_cases.clear();
        self.fn_local_variant_cases.clear();
    }
}
