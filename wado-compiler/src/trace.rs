//! Developer tracing. `WADO_TRACE` selects the targets (a comma-separated list,
//! or `*`); the host selects where they land, by installing a [`TraceSink`].
//! On `wasm32-unknown-unknown` the filter is always empty.

use std::fmt::Arguments;
use std::sync::OnceLock;

#[derive(Debug, Default)]
pub struct TraceFilter {
    all: bool,
    targets: Vec<String>,
}

impl TraceFilter {
    /// Returns `true` when traces tagged with `target` are wanted.
    pub fn enabled(&self, target: &str) -> bool {
        self.all || self.targets.iter().any(|t| t == target)
    }
}

static FILTER: OnceLock<TraceFilter> = OnceLock::new();

/// Get the process-wide trace filter, parsing `WADO_TRACE` on first call.
pub fn filter() -> &'static TraceFilter {
    FILTER.get_or_init(|| parse_filter(std::env::var("WADO_TRACE").ok().as_deref()))
}

/// Parse a comma-separated env-var value, trimming each entry and dropping the
/// empty ones. The trace filter and the `WADO_DUMP_PASS_*` checks share it.
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

/// Where a developer trace lands. A host installs one with [`set_sink`].
pub trait TraceSink: Send + Sync {
    /// Write one trace line. The newline is the sink's to add.
    fn trace(&self, line: &str);
}

static SINK: OnceLock<&'static dyn TraceSink> = OnceLock::new();

/// Install the process-wide trace sink. The first host to call wins, so a
/// process running many compiles keeps one destination.
pub fn set_sink(sink: &'static dyn TraceSink) {
    let _ = SINK.set(sink);
}

/// Write one line to the installed sink, dropping it when no host installed
/// one. The caller has already decided the line is wanted.
pub fn write(line: &str) {
    if let Some(sink) = SINK.get() {
        sink.trace(line);
    }
}

/// Write a developer trace when `WADO_TRACE` selects `target`. Prefer
/// [`compiler_trace!`], which keeps the formatting lazy.
pub fn emit(target: &str, args: Arguments<'_>) {
    if filter().enabled(target) {
        write(&format!("[{target}] {args}"));
    }
}

/// Emit a developer trace if the given target is enabled by `WADO_TRACE`.
///
/// ```ignore
/// compiler_trace!("sroa_variant_return", "rewrite return at {span:?}");
/// ```
#[macro_export]
macro_rules! compiler_trace {
    ($target:expr, $($arg:tt)*) => {
        $crate::trace::emit($target, format_args!($($arg)*))
    };
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
