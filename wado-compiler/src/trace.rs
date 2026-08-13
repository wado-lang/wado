//! Compiler-internal tracing for development debugging, selected by the
//! `WADO_TRACE` env var — a comma-separated target list, or `*` — and written to
//! stderr under a `[target]` prefix. `compiler_trace!` expands to a guarded
//! `eprintln!` and is for *developer* diagnostics only; user-facing ones still
//! flow through `Logger`. On `wasm32-unknown-unknown` the filter is always empty.

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

/// Parse a comma-separated env-var value into a Vec, dropping empty
/// entries and trimming whitespace. Used by callers that cache the
/// result themselves (typically a `OnceLock`); this helper exists so
/// `trace::filter()`-style filters and `optimize::pass_dump`-style
/// per-pass checks share the same parse rules.
pub fn parse_env_list(raw: Option<&str>) -> Vec<String> {
    raw.unwrap_or("")
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
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
/// compiler_trace!("sroa_variant_return", "rewrite return at {span:?}");
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
        assert!(f.enabled("sroa_variant_return"));
    }

    #[test]
    fn comma_separated_list() {
        let f = parse_filter(Some("sroa_variant_return,inline"));
        assert!(f.enabled("sroa_variant_return"));
        assert!(f.enabled("inline"));
        assert!(!f.enabled("dce"));
    }

    #[test]
    fn whitespace_is_tolerated() {
        let f = parse_filter(Some(" sroa_variant_return , inline "));
        assert!(f.enabled("sroa_variant_return"));
        assert!(f.enabled("inline"));
    }

    #[test]
    fn empty_entries_are_dropped() {
        let f = parse_filter(Some(",,sroa_variant_return,,"));
        assert!(f.enabled("sroa_variant_return"));
    }
}
