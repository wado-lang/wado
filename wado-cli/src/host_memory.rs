//! Whether the allocator keeps the memory it freed, rather than handing it
//! back to the OS.
//!
//! mimalloc purges a freed page after a short delay, and a compile frees and
//! reallocates enough that building those pages again cost Gale's
//! `codegen_test` a sixteenth of its wall time and a quarter of its kernel time.
//! A long-running `serve` or `lsp` purges, so its resident set falls after a
//! compile.
//!
//! The setting is process-wide. A page freed while memory is retained is never
//! queued for a purge: it stays resident until it is reused.

/// What the allocator does with the memory it frees.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FreedMemory {
    /// Kept resident for reuse.
    Retain,
    /// Handed back to the OS on mimalloc's own delay.
    Return,
}

pub fn set_freed_memory(policy: FreedMemory) {
    #[cfg(feature = "mimalloc")]
    mimalloc_purge::set_freed_memory(policy);
    #[cfg(not(feature = "mimalloc"))]
    let _ = policy;
}

#[cfg(feature = "mimalloc")]
mod mimalloc_purge {
    use std::ffi::c_long;
    use std::sync::OnceLock;

    use libmimalloc_sys::{mi_option_get, mi_option_set, mi_option_t};

    use super::FreedMemory;

    /// `mi_option_purge_delay` in mimalloc.h, v2 and v3 alike; the binding
    /// names no constant for it.
    const PURGE_DELAY: mi_option_t = 15;
    /// A delay below zero purges nothing.
    const NEVER: c_long = -1;

    /// The delay mimalloc started with, which purging restores.
    static DEFAULT_DELAY: OnceLock<c_long> = OnceLock::new();

    pub(super) fn set_freed_memory(policy: FreedMemory) {
        // SAFETY: an option is a word mimalloc reads at each purge decision.
        let default = *DEFAULT_DELAY.get_or_init(|| unsafe { mi_option_get(PURGE_DELAY) });
        let delay = match policy {
            FreedMemory::Retain => NEVER,
            FreedMemory::Return => default,
        };
        // SAFETY: as above; setting one is allowed at any time.
        unsafe { mi_option_set(PURGE_DELAY, delay) };
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn purging_restores_the_starting_delay() {
            // SAFETY: as in `retain_freed`.
            let start = unsafe { mi_option_get(PURGE_DELAY) };
            set_freed_memory(FreedMemory::Retain);
            assert_eq!(unsafe { mi_option_get(PURGE_DELAY) }, NEVER);
            set_freed_memory(FreedMemory::Return);
            assert_eq!(unsafe { mi_option_get(PURGE_DELAY) }, start);
        }
    }
}
