//! Compiler-internal tracing for development debugging.
//!
//! Targets are selected by the `WADO_TRACE` env var (comma-separated list,
//! or `*` for everything). Output goes to stderr with a `[target]` prefix.
//! Inactive targets cost one env-var lookup (cached on first access) and a
//! linear scan over the configured target list — fine for any rate that
//! makes sense in a compiler pass.
//!
//! `compiler_trace!` is for *developer* diagnostics only; user-facing
//! diagnostics still flow through `Logger` / `CompilerHost::emit_diagnostic`.
//!
//! Examples (all from a development shell):
//!
//! ```sh
//! WADO_TRACE=sroa_return cargo run --bin wado -- compile foo.wado
//! WADO_TRACE=sroa_return,inline cargo run --bin wado -- compile foo.wado
//! WADO_TRACE='*' cargo run --bin wado -- compile foo.wado
//! ```
//!
//! The macro expands to a guarded `eprintln!`. On `wasm32-unknown-unknown`
//! `std::env::var` returns `Err(NotPresent)` so the filter is empty and
//! no output is produced.

use std::sync::OnceLock;

#[derive(Debug, Default)]
pub struct TraceFilter {
    all: bool,
    targets: Vec<String>,
}

impl TraceFilter {
    /// Returns `true` when traces tagged with `target` should be printed.
    pub fn enabled(&self, target: &str) -> bool {
        self.all || self.targets.iter().any(|t| t == target)
    }
}

static FILTER: OnceLock<TraceFilter> = OnceLock::new();

/// Get the process-wide trace filter, parsing `WADO_TRACE` on first call.
pub fn filter() -> &'static TraceFilter {
    FILTER.get_or_init(|| parse_filter(std::env::var("WADO_TRACE").ok().as_deref()))
}

fn parse_filter(raw: Option<&str>) -> TraceFilter {
    let mut all = false;
    let mut targets = Vec::new();
    for part in raw.unwrap_or("").split(',') {
        let p = part.trim();
        if p.is_empty() {
            continue;
        }
        if p == "*" {
            all = true;
        } else {
            targets.push(p.to_string());
        }
    }
    TraceFilter { all, targets }
}

/// Emit a developer trace to stderr if the given target is enabled by
/// `WADO_TRACE`.
///
/// ```ignore
/// compiler_trace!("sroa_return", "rewrite return at {span:?}");
/// ```
#[macro_export]
macro_rules! compiler_trace {
    ($target:expr, $($arg:tt)*) => {{
        let filter = $crate::trace::filter();
        if filter.enabled($target) {
            eprintln!("[{}] {}", $target, format_args!($($arg)*));
        }
    }};
}

#[cfg(test)]
mod tests {
    use super::parse_filter;

    #[test]
    fn empty_filter_enables_nothing() {
        let f = parse_filter(None);
        assert!(!f.enabled("anything"));
    }

    #[test]
    fn star_enables_everything() {
        let f = parse_filter(Some("*"));
        assert!(f.enabled("anything"));
        assert!(f.enabled("sroa_return"));
    }

    #[test]
    fn comma_separated_list() {
        let f = parse_filter(Some("sroa_return,inline"));
        assert!(f.enabled("sroa_return"));
        assert!(f.enabled("inline"));
        assert!(!f.enabled("dce"));
    }

    #[test]
    fn whitespace_is_tolerated() {
        let f = parse_filter(Some(" sroa_return , inline "));
        assert!(f.enabled("sroa_return"));
        assert!(f.enabled("inline"));
    }

    #[test]
    fn empty_entries_are_dropped() {
        let f = parse_filter(Some(",,sroa_return,,"));
        assert!(f.enabled("sroa_return"));
    }
}
