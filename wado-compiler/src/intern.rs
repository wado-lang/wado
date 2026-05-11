//! Pointer-identity string interning.
//!
//! [`InternedStr`] is an `Arc<str>` newtype whose `Eq` / `Hash` are
//! defined by pointer identity, making `clone` / `eq` / `hash` all O(1).
//! [`StringInterner`] is a generic intern pool that canonicalises
//! `&str` content into a unique `Arc<str>`.
//!
//! # Soundness contract
//!
//! Two `InternedStr` values compare equal iff their underlying
//! `Arc<str>` pointers are equal. For this to coincide with content
//! equality, every `InternedStr` that may be compared with another must
//! share the same canonical `Arc<str>` for any given content. The crate
//! maintains this invariant by:
//!
//! - constructing all `InternedStr` through a single [`StringInterner`]
//!   pool (per logical scope, e.g. one per compilation), and
//! - adopting any `LazyLock<Arc<str>>` statics (well-known names) into
//!   that same pool via [`StringInterner::with_well_known_arcs`] at
//!   construction, so `interner.intern("prelude")` returns the same
//!   `Arc` as a statically-built [`InternedStr`] for `"prelude"`.
//!
//! `InternedStr::from_arc` is `pub(crate)` so external callers cannot
//! bypass this discipline.

use crate::hashmap::IndexSet;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::ops::Deref;
use std::sync::Arc;

/// Interned string with pointer-identity equality and hashing.
///
/// See the [module-level docs][`crate::intern`] for the soundness
/// contract: pointer identity is content identity only when all
/// comparable values are produced by the same [`StringInterner`] (or
/// adopted from a shared `LazyLock<Arc<str>>` static registered with
/// that interner).
#[derive(Debug, Clone)]
pub struct InternedStr(Arc<str>);

impl InternedStr {
    /// Build from a raw `Arc<str>`. Restricted to this crate so external
    /// callers cannot bypass the interner.
    pub(crate) fn from_arc(arc: Arc<str>) -> Self {
        Self(arc)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Deref for InternedStr {
    type Target = str;
    fn deref(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for InternedStr {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl PartialEq for InternedStr {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}
impl Eq for InternedStr {}

impl Hash for InternedStr {
    fn hash<H: Hasher>(&self, state: &mut H) {
        (Arc::as_ptr(&self.0).cast::<()>() as usize).hash(state);
    }
}

impl fmt::Display for InternedStr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl PartialEq<str> for InternedStr {
    fn eq(&self, other: &str) -> bool {
        &*self.0 == other
    }
}

impl PartialEq<&str> for InternedStr {
    fn eq(&self, other: &&str) -> bool {
        &*self.0 == *other
    }
}

/// Generic pointer-identity string interner.
///
/// Owns the canonical `Arc<str>` for each interned content; calls to
/// [`Self::intern`] with equal `&str` return pointer-equal
/// [`InternedStr`] values. Clones of an `InternedStr` are O(1) refcount
/// bumps and never reallocate the underlying buffer.
///
/// To share identity with externally-defined `LazyLock<Arc<str>>`
/// statics, construct the interner via [`Self::with_well_known_arcs`]
/// so those `Arc`s are adopted up-front.
#[derive(Debug)]
pub struct StringInterner {
    strings: IndexSet<Arc<str>>,
}

impl StringInterner {
    pub fn new() -> Self {
        Self::with_well_known_arcs(Vec::new())
    }

    /// Create an interner that adopts the given canonical `Arc<str>`
    /// values. Any subsequent [`Self::intern`] call with content equal
    /// to one of these arcs returns the adopted arc rather than
    /// allocating a fresh one — making [`InternedStr`] values produced
    /// by this interner pointer-equal to ones constructed directly from
    /// those statics.
    pub fn with_well_known_arcs(arcs: Vec<Arc<str>>) -> Self {
        let mut strings = IndexSet::with_capacity_and_hasher(arcs.len(), rustc_hash::FxBuildHasher);
        for arc in arcs {
            strings.insert(arc);
        }
        Self { strings }
    }

    pub fn intern(&mut self, s: &str) -> InternedStr {
        if let Some(existing) = self.strings.get(s) {
            return InternedStr::from_arc(existing.clone());
        }
        let arc: Arc<str> = Arc::from(s);
        self.strings.insert(arc.clone());
        InternedStr::from_arc(arc)
    }

    pub fn len(&self) -> usize {
        self.strings.len()
    }

    pub fn is_empty(&self) -> bool {
        self.strings.is_empty()
    }
}

impl Default for StringInterner {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intern_returns_pointer_equal_values_for_equal_content() {
        let mut interner = StringInterner::new();
        let a = interner.intern("hello");
        let b = interner.intern("hello");
        assert_eq!(a, b);
        assert!(Arc::ptr_eq(&a.0, &b.0));
    }

    #[test]
    fn intern_distinguishes_different_content() {
        let mut interner = StringInterner::new();
        let a = interner.intern("hello");
        let b = interner.intern("world");
        assert_ne!(a, b);
    }

    #[test]
    fn adopts_well_known_arcs() {
        let well_known: Arc<str> = Arc::from("prelude");
        let mut interner = StringInterner::with_well_known_arcs(vec![well_known.clone()]);
        let interned = interner.intern("prelude");
        assert!(Arc::ptr_eq(&interned.0, &well_known));
    }

    #[test]
    fn hash_matches_pointer_identity() {
        use crate::hashmap::IndexSet;
        let mut interner = StringInterner::new();
        let a = interner.intern("key");
        let b = interner.intern("key");

        let mut set: IndexSet<InternedStr> = IndexSet::default();
        set.insert(a);
        assert!(set.contains(&b));
    }

    #[test]
    fn partial_eq_against_str() {
        let mut interner = StringInterner::new();
        let s = interner.intern("foo");
        assert_eq!(s, *"foo");
        assert_eq!(s, "foo");
    }

    #[test]
    fn len_counts_well_known_and_interned() {
        let mut interner =
            StringInterner::with_well_known_arcs(vec![Arc::from("a"), Arc::from("b")]);
        assert_eq!(interner.len(), 2);
        interner.intern("a");
        assert_eq!(interner.len(), 2);
        interner.intern("c");
        assert_eq!(interner.len(), 3);
    }
}
