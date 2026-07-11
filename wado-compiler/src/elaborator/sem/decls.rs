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

use super::super::types::{EnumInfo, FlagsInfo, GenericNewtypeInfo, StructFieldInfo, VariantInfo};

/// A function's canonical signature, resolved once by its module's decl
/// pass: the function's own type params registered as `TypeParam` slots
/// (fn-bound params eagerly realised, effect params installed), all types
/// resolved in the declaring module's perspective. Use sites substitute
/// into this frame; they never re-resolve the signature AST.
#[derive(Clone)]
pub(crate) struct FunctionSig {
    /// Registered `(name, TypeId)` pairs, in registration order — real
    /// params as `TypeParam` slots, fn-bound params as their realised
    /// function type. Effect params are excluded (their own channel).
    /// Empty iff the function declares only effect params (or none).
    pub(crate) type_param_ids: Vec<(String, TypeId)>,
    /// The real (non-effect, non-fn-bound) subset of `type_param_ids` —
    /// the positional `TypeParam` slots substituted by explicit or
    /// inferred type arguments, and the rows of the generic-inference
    /// caches.
    pub(crate) real_type_params: Vec<(String, TypeId)>,
    /// Parameter types in declaration order.
    pub(crate) param_types: Vec<TypeId>,
    pub(crate) param_names: Vec<String>,
    pub(crate) param_is_mut: Vec<bool>,
    /// Default-value expressions (irreducibly AST: re-resolved per call
    /// site under the callee's scope, WEP 2026-04-11).
    pub(crate) param_defaults: Vec<Option<ast::Expr>>,
    /// Declared return type; `None` when the declaration has none.
    pub(crate) return_type: Option<TypeId>,
    /// Declared `with` effects, resolved in the declaring perspective
    /// (effect parameters stay symbolic as `EffectRef::Param`).
    pub(crate) effects: Vec<crate::tir::EffectRef>,
}

impl ModuleDecls {
    /// Carry a snapshot module's decl digests into this compile's
    /// `ModuleSemantics`. The field list lives here, next to the fields —
    /// the driver's program-wide assembly merges the same set, so a digest
    /// added to this method is automatically both snapshot-seeded and
    /// merged; one missed here would silently lose stdlib facts on
    /// snapshot-hit builds only.
    pub(crate) fn clone_digests_from(&mut self, other: &ModuleDecls) {
        self.associated_constants
            .clone_from(&other.associated_constants);
        self.effect_op_sigs.clone_from(&other.effect_op_sigs);
        self.function_sigs = std::rc::Rc::clone(&other.function_sigs);
        self.current_module_globals
            .clone_from(&other.current_module_globals);
    }
}

/// Per-module declaration tables produced by elaboration.
#[derive(Default, Clone)]
pub(crate) struct ModuleDecls {
    /// Canonical signatures of this module's own free functions, recorded
    /// by the decl pass and frozen behind `Rc` — the driver's program-wide
    /// assembly (`TypeSystem::all_function_sigs`) and the stdlib-snapshot
    /// seeding both share the map instead of deep-cloning every signature.
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
    /// This module's own impl-associated constants, keyed by the constant's
    /// canonical identity: `(type-declaring module, "Type::CONST")`, with the
    /// impl target's prefix canonicalized in this module's scope
    /// ([`super::super::Elaborator::canonical_decl_key`]). The value carries
    /// the impl-declaring module (reify walks the value expr under that
    /// perspective), the resolved const type, and the value expression
    /// (irreducibly AST). The driver merges every module's map into
    /// `TypeSystem::all_associated_constants` between the decl and body
    /// passes; canonical keys make cross-module collisions impossible, so
    /// no shadowing or alias registration is needed.
    pub(crate) associated_constants:
        IndexMap<(ModuleSource, String), (ModuleSource, TypeId, ast::Expr)>,
    /// This module's own interface / resource operation signatures,
    /// keyed `(decl name, op name)`, resolved once by the decl pass in the
    /// declaring perspective. The driver assembles the program-wide view
    /// (`TypeSystem::all_effect_op_sigs`) between the decl and body passes.
    pub(crate) effect_op_sigs: IndexMap<(String, String), (Vec<TypeId>, Option<TypeId>)>,

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
