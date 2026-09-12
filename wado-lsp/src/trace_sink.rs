//! The stderr [`TraceSink`] every native host installs, so `WADO_TRACE` and
//! `WADO_DUMP_PASS_*` reach a stream wherever the compiler is driven from.

use wado_compiler::{TraceSink, set_trace_sink};

struct StderrTraceSink;

impl TraceSink for StderrTraceSink {
    fn trace(&self, line: &str) {
        eprintln!("{line}");
    }
}

static STDERR_TRACE_SINK: StderrTraceSink = StderrTraceSink;

/// Send developer traces to stderr, beside the diagnostics.
pub fn install_stderr_trace_sink() {
    set_trace_sink(&STDERR_TRACE_SINK);
}
