//! Materialize Wado's value-copy semantics in TIR.
//!
//! This pair of passes runs at the end of the lower phase, after all other
//! TIR-shape transformations (boxing, closure lowering) and before the
//! optimizer. By emitting `$value_copy$T` calls before the optimizer loop,
//! every aliasing edge that downstream passes care about is explicit in
//! the TIR fed to the optimizer — `field_forward` and other flow-sensitive
//! analyses see a self-contained snapshot semantics.
//!
//! - `insert::insert_value_copy_calls` wraps every defensive deep-copy
//!   position in `builtin::copy_value::<T>(x)`.
//! - `synthesize::synthesize_value_copy_funcs` replaces those wrappers
//!   with calls to per-type `$value_copy$T_<id>` helper functions whose
//!   bodies perform a one-level shallow copy.
//!
//! Wrapper elision happens later in `optimize::value_copy_elide`, which
//! runs as a regular pass inside the optimizer fixed-point loop so that
//! newly-exposed `$value_copy$T(...)` patterns from inlining or SROA get
//! collapsed in the same iteration that exposes them.

pub mod insert;
pub mod synthesize;

pub use insert::insert_value_copy_calls;
pub use synthesize::synthesize_value_copy_funcs;

use crate::tir::{ResolvedType, TypeId, TypeTable};

/// True when a value of `type_id` must be deep-copied on assignment or
/// parameter passing.
///
/// Shared between [`insert::insert_value_copy_calls`] (which decides
/// where in user-program TIR to wrap expressions in
/// `builtin::copy_value::<T>(x)`) and [`synthesize::make_field_copy`]
/// (which decides whether a synthesized helper's per-field projection
/// also needs to recurse into a nested value-typed struct). The two
/// callers must agree: when this returns `true`,
/// [`synthesize::generate_copy_function`] emits a `$value_copy$T_<id>`
/// helper, and any caller — user code or another synthesized helper —
/// must route through it. If we wrapped fields whose helper would be
/// identity (`return v;`), the helper would still be typed at WIR
/// level as `(T) -> T` non-null, and any nullable source (e.g. a
/// `None`-initialised `Option<&T>` global) would hit a
/// `(ref X) / nullref` mismatch — an invalid Wasm module at compile
/// time.
pub fn needs_value_copy(type_id: TypeId, type_table: &TypeTable) -> bool {
    match type_table.get(type_id) {
        // Concrete structs need a field-by-field deep copy, except for
        // the `Box<T>` shortcut whose semantics intentionally share
        // the underlying cell.
        ResolvedType::Struct { base_name, .. } => base_name.as_deref() != Some("Box"),
        ResolvedType::GenericInstance {
            name,
            module_source,
            type_args,
        } => {
            if name == "Box" {
                return false;
            }
            if TypeTable::is_tuple_type(name, module_source) {
                // Empty tuples are unit-shaped; non-empty tuples need
                // element-wise deep copy.
                return !type_args.is_empty();
            }
            if name == "Array" {
                return true;
            }
            // `Option<T>` / `Result<T, E>` are reference-shaped variants
            // at WIR level. Their synthesized helper is identity
            // (`return v;`); a non-identity match body that
            // deep-copies the payload would have to be typed at WIR
            // level as `(ref X) -> (ref X)` non-null, but call sites
            // routinely pass nullable sources (e.g. a `None`-initialised
            // `Option<&T>` global lowers to `ref.null`), hitting a
            // `(ref X) / (ref null X)` validation mismatch. References
            // (`Option<&mut T>`) don't need deep-copy anyway — a ref
            // copy intentionally shares the pointee. For value-typed
            // payloads (`Option<String>`, `Option<MyStruct>`),
            // shallow-sharing the payload across copies remains an
            // open bug; tracked separately from the nested-struct /
            // array-of-struct fixes that *are* landed here.
            // Other generic instances: only generic-struct templates
            // need deep copy. Generic-variant templates and
            // generic-resource templates fall through to identity in
            // `synthesize::build_copy_body`, so wrapping them is wasted
            // and triggers the nullable-source mismatch described
            // above.
            type_table.find_struct_type(name, module_source).is_some()
        }
        // `&T` / `&mut T` are reference types: assignment copies the
        // pointer, not the pointee. This is intentional — a struct
        // field of type `&mut T` is meant to share the referenced
        // value with the original, not duplicate it. Stdlib types
        // that need deep-copy semantics use `Array<T>` / `String`
        // (which deep-copy via `array_clone` on their internal
        // `builtin::array`), not `&mut T`.
        ResolvedType::Ref(_) | ResolvedType::MutRef(_) => false,
        // Variants and resources are reference-shaped at WIR level;
        // their copy body is identity (`return v;`).
        ResolvedType::Variant { .. }
        | ResolvedType::Resource { .. }
        | ResolvedType::GenericResource { .. } => false,
        _ => false,
    }
}
