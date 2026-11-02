//! Whether the allocator keeps the memory it freed, rather than handing it
//! back to the OS.
//!
//! mimalloc purges a freed page after a short delay, and a compile frees and
//! reallocates enough that building those pages again cost Gale's
//! `codegen_test` about a twelfth of its CPU, the kernel's share of it halved.
//! A long-running `serve` purges, so its resident set falls after a compile.
//!
//! The setting is process-wide. A page freed while memory is retained is never
//! queued for a purge: it stays resident until it is reused.

/// Retain freed memory, or purge it on mimalloc's own delay.
pub fn retain_freed(retain: bool) {
    #[cfg(feature = "mimalloc")]
    mimalloc_purge::retain_freed(retain);
    #[cfg(not(feature = "mimalloc"))]
    let _ = retain;
}

#[cfg(feature = "mimalloc")]
mod mimalloc_purge {
    use std::ffi::c_long;
    use std::sync::OnceLock;

    use libmimalloc_sys::{mi_option_get, mi_option_set, mi_option_t};

    /// `mi_option_purge_delay` in mimalloc.h, v2 and v3 alike; the binding
    /// names no constant for it.
    const PURGE_DELAY: mi_option_t = 15;
    /// A delay below zero purges nothing.
    const NEVER: c_long = -1;

    /// The delay mimalloc started with, which purging restores.
    static DEFAULT_DELAY: OnceLock<c_long> = OnceLock::new();

    pub(super) fn retain_freed(retain: bool) {
        // SAFETY: an option is a word mimalloc reads at each purge decision.
        let default = *DEFAULT_DELAY.get_or_init(|| unsafe { mi_option_get(PURGE_DELAY) });
        let delay = if retain { NEVER } else { default };
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
            retain_freed(true);
            assert_eq!(unsafe { mi_option_get(PURGE_DELAY) }, NEVER);
            retain_freed(false);
            assert_eq!(unsafe { mi_option_get(PURGE_DELAY) }, start);
        }
    }
}
