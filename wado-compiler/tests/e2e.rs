//! End-to-end tests for Wado compiler
//!
//! These tests compile Wado programs from fixtures/*.wado and run them,
//! verifying the output matches expected values defined in each file's `__DATA__` section.
//!
//! Test fixtures specify the target world via the top-level key in __DATA__:
//! - (default) no world key — runs as `wasi:cli/command`, checks stdout/stderr
//! - `"test": {}` — runs as test world, executes test block exports
//! - `"wasi:http/service": {...}` — runs as HTTP service with request/response spec
//!
//! Helper modules that are imported by tests go in subdirectories
//! (e.g., fixtures/sub/) and are not run as tests themselves.

use wado_compiler_tests as common;

use bytes::Bytes;
use futures::future::{Either, select};
use http_body_util::{BodyExt, Full};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;
use wasmtime::Store;
use wasmtime::component::{Component, ResourceTable};
use wasmtime_wasi::WasiCtxBuilder;
use wasmtime_wasi_http::p3::Request;
use wasmtime_wasi_http::p3::bindings::Service;

use wado_compiler::ast::TodoMark;
use wado_compiler::coverage::{Branch, Coverage, read_from_component, render_plan};
use wado_compiler::{CompilerOptions, OptLevel, OptOverrides};
use wado_host::fixture::{
    CompileInputs, CoverageSpec, GOLDEN_OPT_LEVEL, compile_inputs, data_section, has_wir_golden,
    wir_golden,
};

/// Where each fixture's WIR golden lives, written by its `GOLDEN_OPT_LEVEL` run.
const GOLDEN_DIR: &str = "generated/fixtures";

// ---------------------------------------------------------------------------
// __DATA__ spec
// ---------------------------------------------------------------------------

fn http_method_default() -> String {
    "GET".to_string()
}

fn http_path_default() -> String {
    "/".to_string()
}

/// HTTP request input for `"wasi:http/service"` tests
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct HttpRequestSpec {
    /// HTTP method (default: `"GET"`)
    #[serde(default = "http_method_default")]
    method: String,

    /// Request path + query string, e.g. `"/hello?name=Alice"` (default: `"/"`)
    #[serde(default = "http_path_default")]
    path: String,

    /// Request headers as `[[name, value], ...]`
    #[serde(default)]
    headers: Vec<[String; 2]>,

    /// Request body as UTF-8 string (default: empty)
    #[serde(default)]
    body: Option<String>,
}

impl Default for HttpRequestSpec {
    fn default() -> Self {
        Self {
            method: http_method_default(),
            path: http_path_default(),
            headers: Vec::new(),
            body: None,
        }
    }
}

/// Expected HTTP response and injected request for `"wasi:http/service"` tests.
///
/// Example:
/// ```json
/// {
///   "wasi:http/service": {
///     "request": { "method": "POST", "path": "/api", "body": "hello" },
///     "status": 200,
///     "body": "world",
///     "headers_contain": [["content-type", "text/plain"]]
///   }
/// }
/// ```
#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct HttpServiceSpec {
    /// Injected HTTP request (defaults to `GET /`)
    #[serde(default)]
    request: HttpRequestSpec,

    /// Expected HTTP status code
    #[serde(default)]
    status: Option<u16>,

    /// Expected response body (exact UTF-8 match)
    #[serde(default)]
    body: Option<String>,

    /// Strings that must appear in the response body
    #[serde(default)]
    body_contains: Vec<String>,

    /// Response headers that must be present with the given value: `[[name, value], ...]`
    #[serde(default)]
    headers_contain: Vec<[String; 2]>,
}

/// Test world spec for `"test": {}` — no sub-fields, presence signals the test world.
#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct TestWorldSpec {}

/// Expected test results from __DATA__ section (JSON format)
#[derive(Debug, Deserialize, Default)]
struct TestSpec {
    /// Test world: presence of this key (e.g. `"test": {}`) runs test block exports.
    #[serde(rename = "test")]
    #[serde(default)]
    test_world: Option<TestWorldSpec>,

    /// Expected stdout (exact match) — CLI / test worlds
    #[serde(default)]
    stdout: Option<String>,

    /// Expected stderr (exact match) — CLI world
    #[serde(default)]
    stderr: Option<String>,

    /// Strings that must be contained in stdout
    #[serde(default)]
    stdout_contains: Vec<String>,

    /// Strings that must be contained in stderr
    #[serde(default)]
    stderr_contains: Vec<String>,

    /// Substrings that must each appear in some compile-time warning message
    /// (e.g. `DeadFunction` / `DeadGlobal`). Warnings are emitted during
    /// elaboration (pre-optimize), so they are identical across optimization
    /// levels and checked uniformly.
    #[serde(default)]
    warnings_contains: Vec<String>,

    /// Substrings that must NOT appear in any compile-time warning message —
    /// a false-positive guard (e.g. a used item must not be reported dead).
    #[serde(default)]
    warnings_not_contains: Vec<String>,

    /// Whether the program is expected to trap — CLI world
    #[serde(default)]
    trapped: bool,

    /// Expected `wasi:cli/exit` status code (CLI world). A clean exit is recorded
    /// here, not as a trap, so `trapped` stays `false`.
    #[serde(default)]
    exit_code: Option<i32>,

    /// Expected compile error message (substring match).
    /// If set, the test expects compilation to fail with this message.
    #[serde(default)]
    compile_error: Option<String>,

    /// How many times `compile_error` must occur in the reported message. One
    /// fault is one diagnostic, and a signature walked twice used to say it
    /// twice.
    #[serde(default)]
    compile_error_count: Option<usize>,

    /// Substrings that must each appear in *some* error diagnostic. Where
    /// `compile_error` pins the one the compilation failed with, this reaches
    /// the ones behind it.
    #[serde(default)]
    compile_errors_contains: Vec<String>,

    /// Substrings no error diagnostic may contain. One fault is one
    /// diagnostic, so this pins the follow-on a recovery path used to add.
    #[serde(default)]
    compile_errors_not_contains: Vec<String>,

    /// `Code`s that must each be raised, spelled as the compiler prints them
    /// (`MODULE_NOT_FOUND`). A message is wording and may be reworded; the code
    /// is the fault's identity, so a rejection states which one it is.
    #[serde(default)]
    compile_error_codes: Vec<String>,

    /// Preopened directories, each `[template, guest_path]`. Every preopen is a
    /// fresh temp dir (see `prepare_preopened_dirs`). `template` seeds it:
    /// `""` for empty scratch, or a directory to copy in, relative to the
    /// fixture's own as `dependencies` paths are.
    #[serde(default)]
    preopened_dirs: Vec<[String; 2]>,

    /// Skip this test under `-Os` (e.g. tests that rely on debug names).
    #[serde(default)]
    skip_os: bool,

    /// Stdin data to pipe into the program.
    /// If set, the program's stdin will receive this UTF-8 string.
    #[serde(default)]
    stdin: Option<String>,

    /// HTTP world spec: request injection + response expectations.
    /// Presence of this key implies `world = "wasi:http/service"`.
    #[serde(rename = "wasi:http/service")]
    http_service: Option<HttpServiceSpec>,

    /// The keys that say how to compile the fixture, shared with the golden
    /// dumps.
    #[serde(flatten)]
    compile: CompileInputs,

    /// The keys neither this struct nor `compile` reads. `flatten` rules out
    /// `deny_unknown_fields`, so a misspelt or stale key lands here instead of
    /// passing as unset. It must follow `compile`, which takes its own keys
    /// first.
    #[serde(flatten)]
    unknown: indexmap::IndexMap<String, serde_json::Value>,

    /// Mock responses for outgoing HTTP requests (keyed by URL or path).
    /// When present, any `wasi:http/client#send` from the guest will be
    /// intercepted and matched against these entries.
    #[serde(default)]
    outgoing_mocks: indexmap::IndexMap<String, common::OutgoingMockResponse>,

    /// Mock responses for `wasi:tls` handshakes (keyed by `server_name`).
    /// Any `Connector::connect(_, server_name)` is matched against these
    /// entries. Unmatched server names fail the handshake with a clear error
    /// so tests cannot silently reach the real network.
    #[serde(default)]
    tls_mocks: indexmap::IndexMap<String, common::TlsMockResponse>,

    // --- WIR pattern expectations ---
    // An expectation states the work of the passes `wir_pass` names under its
    // key, so it must fail on the WIR compiled with them skipped. `-O0` runs
    // no pass but DCE, which codegen needs, so an expectation under `O0` states
    // what lowering emits: one the run cannot observe, such as a resource drop
    // or a store width.
    /// Patterns that must appear in WIR output at -O0
    #[serde(rename = "wir_expect:O0", default)]
    wir_expect_o0: Vec<String>,
    /// Patterns that must NOT appear in WIR output at -O0
    #[serde(rename = "wir_not_expect:O0", default)]
    wir_not_expect_o0: Vec<String>,

    /// Patterns that must appear in WIR output at -O1
    #[serde(rename = "wir_expect:O1", default)]
    wir_expect_o1: Vec<String>,
    /// Patterns that must NOT appear in WIR output at -O1
    #[serde(rename = "wir_not_expect:O1", default)]
    wir_not_expect_o1: Vec<String>,

    /// Patterns that must appear in WIR output at -O2
    #[serde(rename = "wir_expect:O2", default)]
    wir_expect_o2: Vec<String>,
    /// Patterns that must NOT appear in WIR output at -O2
    #[serde(rename = "wir_not_expect:O2", default)]
    wir_not_expect_o2: Vec<String>,

    /// Patterns that must appear in WIR output at -O3
    #[serde(rename = "wir_expect:O3", default)]
    wir_expect_o3: Vec<String>,
    /// Patterns that must NOT appear in WIR output at -O3
    #[serde(rename = "wir_not_expect:O3", default)]
    wir_not_expect_o3: Vec<String>,

    /// Patterns that must appear in WIR output at -Os
    #[serde(rename = "wir_expect:Os", default)]
    wir_expect_os: Vec<String>,
    /// Patterns that must NOT appear in WIR output at -Os
    #[serde(rename = "wir_not_expect:Os", default)]
    wir_not_expect_os: Vec<String>,

    /// Narrows every WIR pattern to one function, named by any substring of it.
    /// A short pattern is otherwise answerable by any function in the dump.
    #[serde(rename = "wir_scope", default)]
    wir_scope: Option<String>,

    /// WIR lines, by the optimization level that emits them (`"O2"`), within
    /// `wir_scope`. Where a pattern says only whether
    /// a shape is there, a line counts how many times, and says what else that
    /// line holds.
    #[serde(default)]
    wir_lines: indexmap::IndexMap<String, Vec<TextMatch>>,

    /// The passes and decisions whose work the WIR expectations under each
    /// level (`"O2"`) are, named as `WADO_LIST_PASSES` lists them. The run
    /// compiles again with them skipped, where each of those expectations must
    /// fail with all of them skipped or with one alone, unless a twin under
    /// `wir_contrast` fails it: one that holds anyway measures nothing.
    #[serde(default)]
    wir_pass: indexmap::IndexMap<String, Vec<String>>,

    /// Twins of the fixture by the same keys, each a set of source edits from
    /// a text the program holds once to its replacement. Every twin fails some
    /// WIR expectation under its key. One saying what a pass must leave
    /// standing is measured by the twin that makes the pass do it, which shows
    /// the pattern can see the shape it rules out.
    #[serde(default)]
    wir_contrast: indexmap::IndexMap<String, Vec<indexmap::IndexMap<String, String>>>,

    /// Optimizer remarks, by the optimization level that emits them (`"O2"`).
    /// Each remark reads `"file:line:col remark: …"`, so `":6:5 remark: a copy"`
    /// pins the position in the fixture.
    #[serde(default)]
    remarks: indexmap::IndexMap<String, Vec<TextMatch>>,

    /// Every region the fixture's `profile` marks, as
    /// `"file:line:col kind mark"`, the mark `unlikely cold`, `unlikely` or
    /// `cold`.
    #[serde(default)]
    marked_regions: Option<Vec<String>>,

    /// Lines the emitted component's WAT must carry. A CM import's shape —
    /// which names an instance type exports, which functions are aliased out of
    /// it — is visible here and nowhere a run reaches.
    #[serde(default)]
    wat_lines: Vec<TextMatch>,

    /// Compile and stop. For a program whose subject is the component the
    /// compiler emits, and whose imports the runner hosts nothing for.
    #[serde(default)]
    compile_only: bool,
}

/// One text the compiler emits, a WAT line or a remark: the substrings it
/// holds, and how many such texts there must be.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct TextMatch {
    /// Substrings that must all appear in one text.
    contains: Vec<String>,

    /// Substrings none of which may appear in it. With `count: 0`, this says
    /// every text holding `contains` holds one of these too.
    #[serde(default)]
    not_contains: Vec<String>,

    /// How many texts must match. Omitted means at least one.
    #[serde(default)]
    count: Option<usize>,
}

impl TextMatch {
    /// Assert this entry against `texts`, each a `what`, printing `context` on
    /// a failure.
    fn assert_in(&self, texts: &[&str], what: &str, test_id: &str, context: &str) {
        let matched = self.matching(texts);
        let wanted = self
            .count
            .map_or("1 or more".to_string(), |exact| exact.to_string());
        assert!(
            self.holds(texts),
            "[{test_id}] expected {wanted} {what}(s) holding {:?} and none of {:?}, found {}:\n{matched:#?}\n\n{context}",
            self.contains,
            self.not_contains,
            matched.len(),
        );
    }

    fn matching<'t>(&self, texts: &[&'t str]) -> Vec<&'t str> {
        texts
            .iter()
            .copied()
            .filter(|text| {
                self.contains.iter().all(|needle| text.contains(needle))
                    && !self.not_contains.iter().any(|needle| text.contains(needle))
            })
            .collect()
    }

    fn holds(&self, texts: &[&str]) -> bool {
        let matched = self.matching(texts).len();
        self.count.map_or(matched > 0, |exact| matched == exact)
    }
}

/// One WIR expectation a fixture states.
enum WirCheck<'a> {
    Expect(&'a str),
    NotExpect(&'a str),
    Line(&'a TextMatch),
}

impl WirCheck<'_> {
    fn holds(&self, wir_text: &str) -> bool {
        match self {
            Self::Expect(pattern) => wir_contains(wir_text, pattern),
            Self::NotExpect(pattern) => !wir_contains(wir_text, pattern),
            Self::Line(wanted) => wanted.holds(&wir_lines(wir_text)),
        }
    }

    fn describe(&self, key: &str) -> String {
        match self {
            Self::Expect(pattern) => format!("wir_expect:{key} {pattern:?}"),
            Self::NotExpect(pattern) => format!("wir_not_expect:{key} {pattern:?}"),
            Self::Line(wanted) => format!("wir_lines.{key} {wanted:?}"),
        }
    }
}

fn wir_lines(wir_text: &str) -> Vec<&str> {
    wir_text.lines().map(str::trim).collect()
}

/// The optimization level a `__DATA__` key such as `"O2"` names.
fn parse_level(name: &str) -> Option<OptLevel> {
    [
        OptLevel::O0,
        OptLevel::O1,
        OptLevel::O2,
        OptLevel::O3,
        OptLevel::Os,
    ]
    .into_iter()
    .find(|&level| common::opt_level_name(level) == name)
}

impl TestSpec {
    /// Get WIR expect/not-expect patterns for a given optimization level.
    /// Returns `(expect, not_expect)` slices.
    fn wir_expectations(&self, opt_level: OptLevel) -> (&[String], &[String]) {
        match opt_level {
            OptLevel::O0 => (&self.wir_expect_o0, &self.wir_not_expect_o0),
            OptLevel::O1 => (&self.wir_expect_o1, &self.wir_not_expect_o1),
            OptLevel::O2 => (&self.wir_expect_o2, &self.wir_not_expect_o2),
            OptLevel::O3 => (&self.wir_expect_o3, &self.wir_not_expect_o3),
            OptLevel::Os => (&self.wir_expect_os, &self.wir_not_expect_os),
        }
    }

    /// The WIR expectations under `level`.
    fn wir_checks(&self, level: OptLevel) -> Vec<WirCheck<'_>> {
        let (expect, not_expect) = self.wir_expectations(level);
        let lines = self
            .wir_lines
            .get(common::opt_level_name(level))
            .into_iter()
            .flatten();
        expect
            .iter()
            .map(|p| WirCheck::Expect(p))
            .chain(not_expect.iter().map(|p| WirCheck::NotExpect(p)))
            .chain(lines.map(WirCheck::Line))
            .collect()
    }

    /// Whether this test reads the WIR it compiles at `opt_level`.
    fn has_wir_expectations(&self, opt_level: OptLevel) -> bool {
        !self.wir_checks(opt_level).is_empty()
    }

    /// Whether `opt_level` runs this fixture. The runner skips on it, and the
    /// WAT guard asks it of `-Os`, which strips the symbols a needle may name.
    fn runs_at(&self, opt_level: OptLevel) -> bool {
        !(self.skip_os && opt_level == OptLevel::Os) && self.compile.compiles_at(opt_level)
    }
}

/// Whether `wir` contains `pattern`, where `{}` stands for a run of digits and
/// `{name}` for one run that every `{name}` in the pattern repeats. A generated
/// local's number is an allocation counter, not what a golden means; that two
/// sites name the same local is. A hole after `_` also matches no suffix at
/// all, since a local takes one only while another shares its name.
fn wir_contains(wir: &str, pattern: &str) -> bool {
    let mut texts = Vec::new();
    let mut holes = Vec::new();
    let mut rest = pattern;
    while let Some((start, name, end)) = next_hole(rest) {
        let text = &rest[..start];
        let suffix = text.strip_suffix('_');
        texts.push(suffix.unwrap_or(text));
        holes.push(Hole {
            name,
            suffix: suffix.is_some(),
        });
        rest = &rest[end..];
    }
    texts.push(rest);
    let mut from = 0;
    while let Some(hit) = wir[from..].find(texts[0]) {
        if holes_match(&wir[from + hit + texts[0].len()..], &holes, &texts[1..]) {
            return true;
        }
        from += hit + 1;
        while from < wir.len() && !wir.is_char_boundary(from) {
            from += 1;
        }
    }
    false
}

/// The first hole in `pattern`: where it starts, its name (empty for `{}`),
/// and where it ends.
fn next_hole(pattern: &str) -> Option<(usize, &str, usize)> {
    pattern.match_indices('{').find_map(|(start, _)| {
        let after = &pattern[start + 1..];
        let name = after.trim_start_matches(|c: char| c.is_ascii_lowercase() || c == '_');
        let name = &after[..after.len() - name.len()];
        after[name.len()..]
            .starts_with('}')
            .then_some((start, name, start + name.len() + 2))
    })
}

/// A `{}` or `{name}` in a WIR pattern. `suffix` is one written after `_`,
/// which matches `_` and digits, or nothing.
struct Hole<'p> {
    name: &'p str,
    suffix: bool,
}

/// Whether `wir` opens with each hole's serial followed by its text, a named
/// hole repeating the serial it took first.
fn holes_match(mut wir: &str, holes: &[Hole<'_>], texts: &[&str]) -> bool {
    let mut taken: Vec<(&str, &str)> = Vec::new();
    for (hole, text) in holes.iter().zip(texts) {
        let run = |s: &str| s.len() - s.trim_start_matches(|c: char| c.is_ascii_digit()).len();
        let len = match wir.strip_prefix('_') {
            Some(after) if hole.suffix && run(after) > 0 => 1 + run(after),
            _ if hole.suffix => 0,
            _ => run(wir),
        };
        if len == 0 && !hole.suffix {
            return false;
        }
        let serial = &wir[..len];
        if !hole.name.is_empty() {
            match taken
                .iter()
                .find(|(taken_name, _)| *taken_name == hole.name)
            {
                Some((_, first)) if *first != serial => return false,
                Some(_) => {}
                None => taken.push((hole.name, serial)),
            }
        }
        let Some(after) = wir[len..].strip_prefix(text) else {
            return false;
        };
        wir = after;
    }
    true
}

#[test]
fn wir_patterns_match_serials_by_hole() {
    let wir = "$a_16 = $a_16 + 1; $b_3 = $a_16;";
    assert!(wir_contains(wir, "$a_{} = $a_{} + 1"));
    assert!(wir_contains(wir, "$a_{p} = $a_{p} + 1"));
    assert!(wir_contains(wir, "$b_{} = $a_{p};"));
    assert!(!wir_contains(wir, "$b_{p} = $a_{p};"));
    assert!(!wir_contains("$a_16 = $a_17 + 1", "$a_{p} = $a_{p} + 1"));
    assert!(wir_contains("{ x }", "{ x }"));
    assert!(wir_contains("select(c, x, 0)", "select(c, x_{}, 0)"));
    assert!(wir_contains("f(i, n - i)", "f(i_{i}, n - i_{i})"));
    assert!(!wir_contains("f(i, n - i_2)", "f(i_{i}, n - i_{i})"));
    assert!(!wir_contains("x_ = 1", "x_{} = 1"));
}

#[test]
fn text_match_not_contains_leaves_out_the_texts_holding_one() {
    let texts = ["canon stream.read $t async", "canon stream.read $u"];
    let sync_reads = |count| TextMatch {
        contains: vec!["canon stream.read ".to_string()],
        not_contains: vec![" async".to_string()],
        count: Some(count),
    };
    sync_reads(1).assert_in(&texts, "WAT line", "unit", "");
    let none = std::panic::catch_unwind(|| sync_reads(0).assert_in(&texts, "WAT line", "unit", ""));
    assert!(none.is_err());
}

fn with_passes(mut spec: TestSpec, passes: &[&str]) -> TestSpec {
    spec.wir_pass.insert(
        "O2".to_string(),
        passes.iter().map(|p| p.to_string()).collect(),
    );
    spec
}

fn no_twin(_: &indexmap::IndexMap<String, String>) -> Option<String> {
    unreachable!("the spec states no twin")
}

#[test]
fn an_expectation_holding_with_its_passes_skipped_is_reported() {
    let spec = with_passes(
        TestSpec {
            wir_expect_o2: vec!["folded".to_string()],
            wir_not_expect_o2: vec!["copy".to_string()],
            ..Default::default()
        },
        &["p"],
    );
    let measured = |wir: &'static str| {
        let compile = move |_: &[String]| Some(wir.to_string());
        std::panic::catch_unwind(|| {
            assert_wir_checks_measured(&compile, &no_twin, &spec, OptLevel::O2, "unit");
        })
        .is_ok()
    };
    assert!(measured("a copy"));
    assert!(!measured("a copy, already folded"));
    assert!(!measured("nothing"));
}

#[test]
fn an_expectation_one_listed_pass_measures_alone_is_measured() {
    // Skipping `inline` too leaves no `$cold` to look for, so only skipping
    // `copy_prop` alone shows that `copy_prop` is what removed it.
    let spec = with_passes(
        TestSpec {
            wir_expect_o2: vec!["inlined".to_string()],
            wir_not_expect_o2: vec!["$cold".to_string()],
            ..Default::default()
        },
        &["copy_prop", "inline"],
    );
    let compile = |skip: &[String]| {
        Some(match skip {
            [one] if one == "copy_prop" => "inlined $cold".to_string(),
            _ => "call".to_string(),
        })
    };
    assert_wir_checks_measured(&compile, &no_twin, &spec, OptLevel::O2, "unit");
    let neither = |_: &[String]| Some("inlined".to_string());
    let held = std::panic::catch_unwind(|| {
        assert_wir_checks_measured(&neither, &no_twin, &spec, OptLevel::O2, "unit");
    });
    assert!(held.is_err());
}

#[test]
fn an_expectation_its_twin_fails_is_measured() {
    // A pass must not hoist `level`: no skip can show the pattern sees a hoist,
    // the twin that makes the pass hoist does.
    let mut spec = with_passes(
        TestSpec {
            wir_expect_o2: vec!["$licm_level".to_string()],
            wir_not_expect_o2: vec!["$licm_value".to_string()],
            ..Default::default()
        },
        &["licm"],
    );
    let twin: indexmap::IndexMap<String, String> = [("bump(c);".to_string(), String::new())]
        .into_iter()
        .collect();
    spec.wir_contrast.insert("O2".to_string(), vec![twin]);
    let compile = |_: &[String]| Some("level value".to_string());
    let hoisting =
        |_: &indexmap::IndexMap<String, String>| Some("$licm_level $licm_value".to_string());
    assert_wir_checks_measured(&compile, &hoisting, &spec, OptLevel::O2, "unit");
    let unchanged = |_: &indexmap::IndexMap<String, String>| Some("$licm_level value".to_string());
    let held = std::panic::catch_unwind(|| {
        assert_wir_checks_measured(&compile, &unchanged, &spec, OptLevel::O2, "unit");
    });
    assert!(held.is_err());
}

#[test]
fn a_twin_edit_names_text_the_program_holds_once() {
    let source = "a(); b();\n__DATA__\n{\"wir_contrast\": {\"O2\": [{\"a();\": \"c();\"}]}}";
    let edit = |from: &str| -> indexmap::IndexMap<String, String> {
        [(from.to_string(), "c();".to_string())]
            .into_iter()
            .collect()
    };
    assert!(twin_source(source, &edit("a();"), "unit").starts_with("c(); b();\n__DATA__"));
    let twice = std::panic::catch_unwind(|| twin_source(source, &edit("();"), "unit"));
    assert!(twice.is_err());
}

#[test]
fn wir_scope_names_one_function_whatever_the_order() {
    let wir = "fn \"m/ascending.wado/$cm_export__run\"() {\n    a;\n}\n\
               fn \"m/ascending.wado/ascending\"(xs) {\n    b;\n}\n\
               fn \"m/ascending.wado/run\"() {\n    c;\n}\n";
    let body = |scope| scope_to_function(wir, scope).map(|b| b.lines().nth(1).unwrap().trim());
    assert_eq!(body("ascending"), Some("b;"));
    assert_eq!(body(".wado/run\""), Some("c;"));
    assert_eq!(body("export"), Some("a;"));
    assert_eq!(body("missing"), None);
    assert_eq!(body("fn"), None);
}

/// The body of the function in `wir` that `scope` names, from its `fn` line to
/// the `}` closing it, or `None` when no name matches. A name ending in `scope`
/// names it, else one merely containing it; either must be the only such name,
/// since the order functions are emitted in is not the fixture's to rely on.
fn scope_to_function<'a>(wir: &'a str, scope: &str) -> Option<&'a str> {
    let mut bodies: Vec<(&str, &str)> = Vec::new();
    let mut start = None;
    let mut offset = 0;
    for line in wir.split_inclusive('\n') {
        match start {
            Some((s, name)) if line.starts_with('}') => {
                bodies.push((name, &wir[s..offset + line.len()]));
                start = None;
            }
            None => {
                if let Some(rest) = line.strip_prefix("fn \"")
                    && let Some(end) = rest.find('"')
                {
                    // Quoted, as a scope may spell it.
                    start = Some((offset, &line[3..=4 + end]));
                }
            }
            _ => {}
        }
        offset += line.len();
    }
    // Returning the tail here would widen the scope to every function below,
    // which is the one thing `wir_scope` exists to stop.
    assert!(
        start.is_none(),
        "WIR function matching {scope:?} is never closed"
    );
    let suffix = scope.trim_end_matches('"');
    let named = |test: &dyn Fn(&str) -> bool| -> Vec<&str> {
        bodies
            .iter()
            .filter(|(name, _)| test(name))
            .map(|(_, body)| *body)
            .collect()
    };
    let mut candidates = named(&|name| name.trim_end_matches('"').ends_with(suffix));
    if candidates.is_empty() {
        candidates = named(&|name| name.contains(scope));
    }
    assert!(
        candidates.len() <= 1,
        "wir_scope {scope:?} names {} functions; qualify it",
        candidates.len()
    );
    candidates.first().copied()
}

// ---------------------------------------------------------------------------
// CLI world verification (shared with test world)
// ---------------------------------------------------------------------------

/// Verify the actual result matches the expected spec
fn verify_result(
    result: &common::WasmRunResult,
    spec: &TestSpec,
    fixture_name: &str,
    opt_level: OptLevel,
) {
    // Check trapped status
    assert_eq!(
        result.trapped, spec.trapped,
        "[{fixture_name}] trapped mismatch: expected {}, got {}\n  stderr: {:?}\n  stdout: {:?}",
        spec.trapped, result.trapped, result.stderr, result.stdout
    );

    if let Some(expected_code) = spec.exit_code {
        assert_eq!(
            result.exit_code,
            Some(expected_code),
            "[{fixture_name}] exit code mismatch: expected {expected_code}, got {:?}\n  stderr: {:?}\n  stdout: {:?}",
            result.exit_code,
            result.stderr,
            result.stdout
        );
    }

    // Check stdout exact match if specified
    if let Some(expected_stdout) = &spec.stdout {
        assert_eq!(
            &result.stdout, expected_stdout,
            "[{fixture_name}] stdout mismatch\n  stderr was: {:?}",
            result.stderr
        );
    }

    // Check stderr exact match if specified
    if let Some(expected_stderr) = &spec.stderr {
        assert_eq!(
            &result.stderr, expected_stderr,
            "[{fixture_name}] stderr mismatch"
        );
    }

    // Check stdout contains
    for expected in &spec.stdout_contains {
        assert!(
            result.stdout.contains(expected),
            "[{fixture_name}] stdout should contain '{expected}', but got:\n{}",
            result.stdout
        );
    }

    // Check stderr contains.
    //
    // `-f bare-asserts` (default at -Os) lowers an assertion failure to a bare
    // `unreachable` trap, so a trapping assert at -Os carries no diagnostic
    // message. When the program trapped exactly as the spec expects and stderr
    // is just the bare trap backtrace (nothing logged before it), accept the
    // stripped message. Explicit `panic(...)` text still precedes the backtrace
    // and is enforced normally (it is not stripped).
    let assert_diagnostic_stripped =
        opt_level == OptLevel::Os && spec.trapped && result.trapped && is_bare_trap(&result.stderr);
    for expected in &spec.stderr_contains {
        assert!(
            result.stderr.contains(expected) || assert_diagnostic_stripped,
            "[{fixture_name}] stderr should contain '{expected}', but got:\n{}",
            result.stderr
        );
    }
}

/// A bare wasm trap with no guest-logged diagnostic before it — what a stripped
/// (`-f bare-asserts`) assertion failure produces. `panic(...)` / `log_stderr`
/// output would precede the host's `error while executing` backtrace.
fn is_bare_trap(stderr: &str) -> bool {
    stderr.trim_start().starts_with("error while executing") && stderr.contains("unreachable")
}

// ---------------------------------------------------------------------------
// HTTP world infrastructure
// ---------------------------------------------------------------------------

/// Multi-threaded tokio runtime for HTTP tests (needs `run_concurrent` / `tokio::spawn`)
static HTTP_RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();

fn http_runtime() -> &'static tokio::runtime::Runtime {
    HTTP_RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("Failed to create HTTP tokio runtime")
    })
}

/// Result of an HTTP request to the Wasm component
struct HttpTestResult {
    status: u16,
    headers: http::HeaderMap,
    body: Vec<u8>,
}

/// Run an HTTP request against a compiled Wasm component
fn run_http_request(
    wasm: Vec<u8>,
    req_spec: &HttpRequestSpec,
    outgoing_mocks: indexmap::IndexMap<String, common::OutgoingMockResponse>,
    tls_mocks: indexmap::IndexMap<String, common::TlsMockResponse>,
) -> anyhow::Result<HttpTestResult> {
    http_runtime().block_on(run_http_request_async(
        wasm,
        req_spec,
        outgoing_mocks,
        tls_mocks,
    ))
}

async fn run_http_request_async(
    wasm: Vec<u8>,
    req_spec: &HttpRequestSpec,
    outgoing_mocks: indexmap::IndexMap<String, common::OutgoingMockResponse>,
    tls_mocks: indexmap::IndexMap<String, common::TlsMockResponse>,
) -> anyhow::Result<HttpTestResult> {
    let engine = common::engine();
    let component = Component::new(engine, &wasm)
        .map_err(|e| anyhow::anyhow!("failed to create component: {e:?}"))?;

    let linker = common::host_linker(&component)?;

    let state = common::WasiState {
        ctx: WasiCtxBuilder::new().build(),
        table: ResourceTable::new(),
        http_ctx: wasmtime_wasi_http::WasiHttpCtx::new(),
        http_hooks: common::TestHttpCtx {
            mocks: outgoing_mocks,
        },
        tls_ctx: common::build_tls_ctx(tls_mocks),
        coverage: common::TestReport::default(),
    };
    let mut store = Store::new(engine, state);
    // Set epoch deadline for timeout enforcement (HTTP tests use 5s default)
    common::limit_store(&mut store, common::DEFAULT_TIMEOUT_MS);

    let service = Service::instantiate_async(&mut store, &component, &linker).await?;

    // Build request from spec
    let method = req_spec
        .method
        .parse::<http::Method>()
        .unwrap_or(http::Method::GET);
    let uri = format!("http://localhost{}", req_spec.path);
    let body_bytes = req_spec.body.as_deref().unwrap_or("").as_bytes().to_vec();

    let mut builder = http::Request::builder().uri(uri).method(method);
    for [name, value] in &req_spec.headers {
        builder = builder.header(name.as_str(), value.as_str());
    }
    let http_req = builder.body(Full::new(Bytes::from(body_bytes)))?;

    let (req, io) = Request::from_http(wasmtime_wasi_http::default_hooks(), http_req);

    let timeout_duration = Duration::from_secs(1);

    // Run handler, request body I/O, and response body collection all inside
    // run_concurrent. This follows wasmtime's own HTTP test pattern
    // (vendor/wasmtime/crates/wasi-http/tests/all/p3/mod.rs::run_http).
    // Everything must be inside run_concurrent so that async host futures
    // (e.g., Client::send mock responses) can be polled by the runtime.
    let handle_result = tokio::time::timeout(timeout_duration, async {
        store
            .run_concurrent(async |store| {
                use std::pin::pin;

                let handler = pin!(async {
                    let res = match service.handle(store, req).await? {
                        Ok(res) => res,
                        Err(err) => return anyhow::Ok(Err(err)),
                    };
                    let res = store.with(|store| res.into_http(store, async { Ok(()) }))?;
                    let (parts, body) = res.into_parts();
                    let body = body
                        .collect()
                        .await
                        .map_err(|e| anyhow::anyhow!("failed to collect body: {e}"))?;
                    anyhow::Ok(Ok(http::Response::from_parts(parts, body)))
                });
                let io = pin!(async {
                    io.await
                        .map_err(|e| anyhow::anyhow!("request body I/O: {e}"))
                });
                // The handler's result is authoritative. The request-body I/O
                // future runs concurrently only so host-side body delivery can
                // progress; it legitimately completes early once the guest
                // finishes with the request (e.g. drops the `Request` handle,
                // closing the body stream). So if `io` finishes first, surface
                // a genuine body error but otherwise keep awaiting the handler.
                match select(handler, io).await {
                    Either::Left((result, _)) => result,
                    Either::Right((io_result, handler)) => {
                        io_result?;
                        handler.await
                    }
                }
            })
            .await?
    })
    .await
    .map_err(|_| anyhow::anyhow!("HTTP handler timed out after {timeout_duration:?}"))??;
    common::report_fuel_used(&mut store, "http-world", common::DEFAULT_TIMEOUT_MS);

    // Resource-cleanup check: every per-request wasi-http resource handle
    // (the incoming `Request`, `Fields`, `Response`, stream/future ends) must
    // be dropped by the time the handler finishes and its response body has
    // been collected. A surviving table entry means the guest leaked a
    // `resource.drop`; under `wado serve`'s pooled instance reuse this leaks
    // one slot per request and eventually exhausts the component resource
    // table ("resource table has no free keys" — issue #1133).
    assert!(
        store.data().table.is_empty(),
        "[resource leak] the HTTP handler left resources undropped in the \
         component resource table ({:?}). Every per-request Component Model \
         resource must be dropped before the handler returns.",
        store.data().table,
    );

    match handle_result {
        Ok(res) => {
            let status = res.status().as_u16();
            let headers = res.headers().clone();
            let body = res.into_body().to_bytes().to_vec();
            Ok(HttpTestResult {
                status,
                headers,
                body,
            })
        }
        Err(error_code) => Ok(HttpTestResult {
            status: 500,
            headers: http::HeaderMap::new(),
            body: format!("{error_code:?}").into_bytes(),
        }),
    }
}

/// Verify an HTTP test result matches the spec
fn verify_http_result(result: &HttpTestResult, spec: &HttpServiceSpec, fixture_name: &str) {
    if let Some(expected_status) = spec.status {
        assert_eq!(
            result.status,
            expected_status,
            "[{fixture_name}] HTTP status mismatch: expected {expected_status}, got {}; body: {}",
            result.status,
            String::from_utf8_lossy(&result.body),
        );
    }

    if let Some(expected_body) = &spec.body {
        let actual_body = String::from_utf8_lossy(&result.body);
        assert_eq!(
            actual_body,
            expected_body.as_str(),
            "[{fixture_name}] body mismatch"
        );
    }

    for expected in &spec.body_contains {
        let actual_body = String::from_utf8_lossy(&result.body);
        assert!(
            actual_body.contains(expected.as_str()),
            "[{fixture_name}] body should contain '{expected}', but got:\n{actual_body}"
        );
    }

    for [name, value] in &spec.headers_contain {
        let header_name = http::header::HeaderName::from_bytes(name.as_bytes())
            .unwrap_or_else(|_| panic!("[{fixture_name}] invalid header name in spec: {name}"));
        let expected_val = value.as_str();
        let actual_val = result
            .headers
            .get(&header_name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("(missing)");
        assert_eq!(
            actual_val, expected_val,
            "[{fixture_name}] response header '{name}': expected '{expected_val}', got '{actual_val}'"
        );
    }
}

// ---------------------------------------------------------------------------
// Main test dispatch
// ---------------------------------------------------------------------------

/// Run a single fixture test at a specific optimization level
fn run_fixture_test_with_opt(fixture_path: &Path, source: &str, opt_level: OptLevel) {
    let fixture_name = fixture_path
        .file_name()
        .unwrap()
        .to_string_lossy()
        .to_string();
    let opt_name = common::opt_level_name(opt_level);
    let test_id = format!("{fixture_name} ({opt_name})");

    // A fixture with a `__DATA__` section carries explicit expectations; one
    // without is a library-shaped source run under the test world (compile +
    // instantiate, executing its `test` blocks). The latter lets a published
    // library double as a fixture verbatim — see `cm_catalog.wado`.
    let spec: TestSpec = match data_section(source) {
        Some(data_section) => common::parse_data_section(data_section, &test_id),
        None => TestSpec {
            test_world: Some(TestWorldSpec::default()),
            ..Default::default()
        },
    };
    assert!(
        spec.unknown.is_empty(),
        "[{test_id}] __DATA__ names keys the harness does not read: {:?}",
        spec.unknown.keys().collect::<Vec<_>>()
    );

    if !spec.runs_at(opt_level) {
        eprintln!(
            "[{test_id}] skipped ({} excluded)",
            common::opt_level_name(opt_level)
        );
        return;
    }

    // Check if source has #![TODO] to determine panic recovery strategy.
    // For test world: individual tests are already marked #[TODO] by the elaborator,
    // so no special handling is needed here.
    // For CLI/HTTP worlds: runtime failures in #![TODO] modules need catch_unwind.
    let is_todo_module = wado_compiler::parse(source).ast.has_todo();

    if is_todo_module {
        eprintln!("[{test_id}] #![TODO] module — expecting failure");
        let test_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_normal_test(fixture_path, source, opt_level, &spec, &test_id);
        }));
        match test_result {
            Ok(()) if spec.test_world.is_some() => {
                // Test world: every test is marked `test-todo-*` by the elaborator.
                // Reaching Ok means each one trapped as expected — pending, not resolved.
            }
            Ok(()) => {
                // #![TODO] module passed — the feature may now work.
                // This is a hard failure: #![TODO] must be removed.
                panic!(
                    "[{test_id}] #![TODO] module resolved — \
                     remove the #![TODO] attribute from the source file."
                );
            }
            Err(err) => {
                // Every failure but a sentinel is a still-pending TODO.
                reraise_sentinel(err.as_ref(), TodoMark::Module);
                let msg = common::panic_message(err.as_ref());
                eprintln!("[{test_id}] #![TODO] module pending: {msg}");
                return;
            }
        }
    }

    let test_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run_normal_test(fixture_path, source, opt_level, &spec, &test_id);
    }));
    if let Err(err) = test_result {
        reraise_sentinel(err.as_ref(), TodoMark::Test);
        std::panic::resume_unwind(err);
    }
}

/// Re-raise a typed sentinel panic ([`common::TodoResolved`] for a resolved
/// TODO, [`common::InternalCompilerError`] for a compiler crash) as the message
/// it stands for, which the test framework would otherwise report as an opaque
/// `Box<dyn Any>`.
fn reraise_sentinel(err: &(dyn std::any::Any + Send), mark: TodoMark) {
    if let Some(resolved) = err.downcast_ref::<common::TodoResolved>() {
        panic!("{}", resolved.report(mark));
    }
    if let Some(ice) = err.downcast_ref::<common::InternalCompilerError>() {
        panic!("{ice}");
    }
}

/// Run a normal (non-TODO) test under each allocator the fixture names.
fn run_normal_test(
    fixture_path: &Path,
    source: &str,
    opt_level: OptLevel,
    spec: &TestSpec,
    test_id: &str,
) {
    let allocators = spec.compile.allocators();
    let golden = opt_level == GOLDEN_OPT_LEVEL && has_wir_golden(source, &spec.compile);
    for (index, allocator) in allocators.iter().enumerate() {
        let test_id = if allocators.len() > 1 {
            format!("{test_id} [{allocator}]")
        } else {
            test_id.to_string()
        };
        run_with_allocator(
            fixture_path,
            source,
            opt_level,
            spec,
            allocator,
            golden && index == 0,
            &test_id,
        );
    }
}

/// Remove each WIR golden whose fixture is gone or no longer has one. A
/// fixture's own run writes its golden; nothing runs for one that has none.
#[test]
fn prune_orphan_wir_goldens() {
    for entry in std::fs::read_dir(GOLDEN_DIR).unwrap() {
        let golden = entry.unwrap().path();
        let name = golden.file_name().unwrap().to_string_lossy();
        // A host may drop its own files here (`.DS_Store`).
        let Some(stem) = name.strip_suffix(".wir.wado") else {
            continue;
        };
        let keeps = std::fs::read_to_string(format!("fixtures/{stem}.wado")).is_ok_and(|source| {
            let inputs = compile_inputs(&source).unwrap_or_else(|e| panic!("{stem}: {e}"));
            has_wir_golden(&source, &inputs)
        });
        if !keeps {
            std::fs::remove_file(&golden).unwrap();
        }
    }
}

/// Rewrite `fixture`'s WIR golden when `wir` differs from it.
fn update_wir_golden(fixture: &Path, wir: &str) {
    let name = fixture.file_name().unwrap().to_string_lossy();
    let stem = name.strip_suffix(".wado").unwrap();
    let golden = Path::new(GOLDEN_DIR).join(format!("{stem}.wir.wado"));
    let text = wir_golden(&format!("wado-compiler/tests/fixtures/{name}"), wir);
    if std::fs::read_to_string(&golden).ok().as_deref() != Some(text.as_str()) {
        std::fs::write(&golden, text)
            .unwrap_or_else(|e| panic!("cannot write {}: {e}", golden.display()));
    }
}

/// Run a normal (non-TODO) test under `allocator`, dispatching to the
/// appropriate world runner, and update the fixture's WIR golden if `golden`.
fn run_with_allocator(
    fixture_path: &Path,
    source: &str,
    opt_level: OptLevel,
    spec: &TestSpec,
    allocator: &str,
    golden: bool,
    test_id: &str,
) {
    // Determine target world from the world key present in the spec
    let target_world = if spec.http_service.is_some() {
        Some("wasi:http/service".to_string())
    } else if spec.test_world.is_some() {
        Some("test".to_string())
    } else {
        None
    };

    assert!(
        spec.compile.lib_world.is_none()
            || (target_world.is_none() && (spec.compile_only || spec.compile_error.is_some())),
        "[{test_id}] a library has no entry point to run: set `compile_only`, and no world key"
    );
    let options = spec.compile.apply(
        CompilerOptions {
            opt_level,
            target_world,
            skip_validation: false,
            retain_wir: golden || spec.has_wir_expectations(opt_level),
            allocator: Some(allocator.to_string()),
            // More than one, so every fixture runs the optimizer's parallel path
            // against expectations no thread count may change.
            parallelism: 2,
            ..Default::default()
        },
        fixture_path.parent().unwrap(),
    );
    let skip_base = options.clone();

    let common::CompiledFixture {
        wasm,
        wir_text,
        warnings,
        errors,
        remarks,
        marked_regions,
    } = common::compile_fixture_on_worker(
        fixture_path.to_path_buf(),
        source.to_string(),
        options,
        spec.compile.host_stubs(),
    );

    // Assert compile-time warnings (e.g. DeadFunction / DeadGlobal). These are
    // emitted during elaboration, so they are identical across optimization
    // levels; match on the message text only.
    let warning_text = || diagnostic_text(&warnings);
    let error_text = || diagnostic_text(&errors);
    for expected in &spec.warnings_contains {
        assert!(
            warnings.iter().any(|w| w.message.contains(expected)),
            "[{test_id}] expected a warning containing {expected:?}\n  warnings: {}",
            warning_text()
        );
    }
    for unexpected in &spec.warnings_not_contains {
        assert!(
            !warnings.iter().any(|w| w.message.contains(unexpected)),
            "[{test_id}] warning must not contain {unexpected:?}\n  warnings: {}",
            warning_text()
        );
    }

    for name in spec.remarks.keys() {
        assert!(
            parse_level(name).is_some(),
            "[{test_id}] `remarks` is keyed by an optimization level, not {name:?}"
        );
    }
    let parse_key = |field: &str, name: &str| {
        parse_level(name).unwrap_or_else(|| {
            panic!("[{test_id}] `{field}` is keyed by an optimization level, not {name:?}")
        })
    };
    for name in spec.wir_lines.keys() {
        parse_key("wir_lines", name);
    }
    for (key, passes) in &spec.wir_pass {
        assert!(
            !passes.is_empty() && !spec.wir_checks(parse_key("wir_pass", key)).is_empty(),
            "[{test_id}] `wir_pass.{key}` names the passes of the WIR expectations under `{key}`"
        );
    }
    for (key, twins) in &spec.wir_contrast {
        assert!(
            !twins.iter().any(indexmap::IndexMap::is_empty)
                && !spec.wir_checks(parse_key("wir_contrast", key)).is_empty(),
            "[{test_id}] `wir_contrast.{key}` holds twins, each editing the source, that the \
             WIR expectations under `{key}` fail on"
        );
    }
    if let Some(expected) = spec.remarks.get(common::opt_level_name(opt_level)) {
        let texts: Vec<&str> = remarks.iter().map(String::as_str).collect();
        for wanted in expected {
            wanted.assert_in(&texts, "remark", test_id, &format!("remarks: {remarks:#?}"));
        }
    }

    for expected in &spec.compile_errors_contains {
        assert!(
            errors.iter().any(|e| e.message.contains(expected)),
            "[{test_id}] expected an error containing {expected:?}\n  errors: {}",
            error_text()
        );
    }

    for unexpected in &spec.compile_errors_not_contains {
        assert!(
            !errors.iter().any(|e| e.message.contains(unexpected)),
            "[{test_id}] error must not contain {unexpected:?}\n  errors: {}",
            error_text()
        );
    }

    for expected in &spec.compile_error_codes {
        assert!(
            errors.iter().any(|e| &e.code == expected),
            "[{test_id}] expected an error raised as {expected}\n  errors: {}",
            error_text()
        );
    }

    // Handle expected compile errors (works for any world)
    if let Some(expected_error) = &spec.compile_error {
        match wasm {
            Err(error_msg) => {
                assert!(
                    error_msg.contains(expected_error),
                    "[{test_id}] compile error mismatch:\n  expected to contain: {expected_error}\n  actual error: {error_msg}"
                );
                if let Some(expected_count) = spec.compile_error_count {
                    assert!(
                        expected_count > 0,
                        "[{test_id}] compile_error_count counts occurrences of compile_error, which the fixture already requires at least one of"
                    );
                    let actual = error_msg.matches(expected_error.as_str()).count();
                    assert_eq!(
                        actual, expected_count,
                        "[{test_id}] expected '{expected_error}' {expected_count} time(s), found {actual}:\n  {error_msg}"
                    );
                }
                return; // Test passed - expected compile error occurred
            }
            Ok(_) => {
                panic!(
                    "[{test_id}] expected compile error containing '{expected_error}', but compilation succeeded"
                );
            }
        }
    }

    let wasm = wasm.unwrap_or_else(|e| {
        panic!("[{test_id}] compilation failed: {e}");
    });
    if golden {
        update_wir_golden(fixture_path, wir_text.as_deref().unwrap());
    }

    // Optional: dump the compiled wasm (and a wat next to it) so the
    // bytes the e2e runner produces can be diffed against `wado compile`.
    // Set `WADO_KEEP_WASM_DIR=/tmp/wado-debug` (directory will be created
    // if missing). Filenames are `<fixture_name>.<opt>.{wasm,wat}`.
    if let Ok(dir) = std::env::var("WADO_KEEP_WASM_DIR") {
        let fixture_name = fixture_path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "unknown".to_string());
        let opt_name = common::opt_level_name(opt_level);
        keep_wasm_artifacts(&dir, &fixture_name, opt_name, &wasm, test_id);
    }

    assert_wat_lines(&wasm, spec, test_id);
    // Every check on the compiler's own output runs before the execution
    // dispatch: one placed after it is silently skipped by `compile_only`.
    assert_wir_expectations(wir_text.as_deref(), spec, opt_level, test_id);
    let compile_variant = |source: String, skip: &[String]| {
        let options = skip_base.clone();
        common::compile_fixture_on_worker(
            fixture_path.to_path_buf(),
            source,
            CompilerOptions {
                opt: OptOverrides {
                    skip_passes: skip.to_vec(),
                    ..options.opt.clone()
                },
                retain_wir: true,
                ..options
            },
            spec.compile.host_stubs(),
        )
        .wir_text
    };
    let compile_skipping = |skip: &[String]| compile_variant(source.to_string(), skip);
    if spec.has_wir_expectations(opt_level) {
        if let Ok(out) = std::env::var("WADO_E2E_WIR_PASS_SEARCH") {
            let normal_wir = wir_text.as_deref().unwrap();
            search_wir_passes(
                &out,
                &compile_skipping,
                normal_wir,
                spec,
                opt_level,
                test_id,
            );
            return;
        }
        let compile_twin = |edits: &indexmap::IndexMap<String, String>| {
            compile_variant(twin_source(source, edits, test_id), &[])
        };
        assert_wir_checks_measured(&compile_skipping, &compile_twin, spec, opt_level, test_id);
    }
    if std::env::var_os("WADO_E2E_WIR_PASS_SEARCH").is_some() {
        return;
    }
    if let Some(expected) = &spec.marked_regions {
        assert!(
            spec.compile.profile.is_some(),
            "[{test_id}] `marked_regions` states what a `profile` marks"
        );
        assert_eq!(&marked_regions, expected, "[{test_id}] marked regions");
    }

    if spec.compile_only {
        return;
    }

    // Dispatch to the appropriate runner based on world
    if let Some(http_spec) = &spec.http_service {
        match run_http_request(
            wasm,
            &http_spec.request,
            spec.outgoing_mocks.clone(),
            spec.tls_mocks.clone(),
        ) {
            Ok(result) => {
                assert!(
                    !spec.trapped,
                    "[{test_id}] expected HTTP handler to trap, but request succeeded with status {}",
                    result.status
                );
                verify_http_result(&result, http_spec, test_id);
            }
            Err(e) => {
                assert!(
                    spec.trapped,
                    "[{test_id}] HTTP error (no trap expected): {e:?}"
                );
            }
        }
    } else if spec.test_world.is_some() {
        let result = common::run_test_world(
            &wasm,
            test_id,
            spec.outgoing_mocks.clone(),
            spec.tls_mocks.clone(),
        )
        .unwrap_or_else(|e| {
            panic!("[{test_id}] test world error: {e:?}");
        });
        verify_result(&result, spec, test_id, opt_level);
        if let Some(coverage) = &spec.compile.coverage {
            assert_coverage(
                &wasm,
                fixture_path,
                &result.coverage_counts,
                coverage,
                test_id,
            );
        }
    } else {
        // Default: wasi:cli/command. `_temp_dirs` must outlive the run: dropping
        // a `TempDir` deletes it from disk.
        let (dirs, _temp_dirs) =
            prepare_preopened_dirs(fixture_path, &spec.preopened_dirs, test_id);
        let result = common::run_wasm_with_full_options(
            wasm,
            &dirs,
            spec.stdin.as_deref(),
            spec.outgoing_mocks.clone(),
            spec.tls_mocks.clone(),
        )
        .unwrap_or_else(|e| {
            panic!("[{test_id}] runtime error: {e}");
        });
        verify_result(&result, spec, test_id, opt_level);
    }
}

/// Check what the fixture's tests left unrun in the fixture against `expected`.
fn assert_coverage(
    wasm: &[u8],
    fixture_path: &Path,
    per_test: &[BTreeMap<u32, i64>],
    expected: &CoverageSpec,
    test_id: &str,
) {
    let plans = read_from_component(wasm)
        .unwrap_or_else(|| panic!("[{test_id}] the component carries no coverage section"));
    let mut coverage = Coverage::default();
    let registered = coverage
        .register(&plans, |plan| plan.name.clone())
        .unwrap_or_else(|mismatch| panic!("[{test_id}] plans disagree on {}", mismatch.path));
    for counts in per_test {
        coverage.record(&registered, counts, None);
    }
    let name = fixture_path.file_name().unwrap().to_string_lossy();
    let file = coverage
        .files
        .get(name.as_ref())
        .unwrap_or_else(|| panic!("[{test_id}] no plan for {name}"));

    let lines = file.uncovered_lines();
    let branches: Vec<String> = file
        .branches()
        .iter()
        .filter(|b| !b.taken())
        .map(Branch::describe)
        .collect();
    let functions: Vec<String> = file
        .functions()
        .into_iter()
        .filter(|(_, count)| *count == 0)
        .map(|(f, _)| f.name.clone())
        .collect();
    let plan = || render_plan(&file.plan);
    assert_eq!(
        lines,
        expected.uncovered_lines,
        "[{test_id}] uncovered lines\n{}",
        plan()
    );
    assert_eq!(
        branches,
        expected.uncovered_branches,
        "[{test_id}] uncovered branches\n{}",
        plan()
    );
    assert_eq!(
        functions,
        expected.uncovered_functions,
        "[{test_id}] uncovered functions\n{}",
        plan()
    );
    if let Some(line_counts) = &expected.line_counts {
        assert_eq!(
            &file.lines(),
            line_counts,
            "[{test_id}] line counts\n{}",
            plan()
        );
    }
}

/// Check the fixture's WIR expectations for `opt_level` and for `*` against
/// the retained WIR.
fn assert_wir_expectations(
    wir_text: Option<&str>,
    spec: &TestSpec,
    opt_level: OptLevel,
    test_id: &str,
) {
    // A golden retains WIR at a level the patterns may not name, where
    // `wir_scope` may name a function that level inlines away.
    if !spec.has_wir_expectations(opt_level) {
        return;
    }
    let wir_text = wir_text.unwrap_or_else(|| {
        panic!("[{test_id}] the fixture states WIR expectations but no WIR was retained")
    });
    let wir_text = match spec.wir_scope.as_deref() {
        None => wir_text,
        Some(scope) => scope_to_function(wir_text, scope).unwrap_or_else(|| {
            panic!(
                "[{test_id}] wir_scope names no function in the WIR\n\
                 scope: {scope}\n\
                 WIR output:\n{wir_text}"
            )
        }),
    };
    let name = common::opt_level_name(opt_level);
    for check in &spec.wir_checks(opt_level) {
        if let WirCheck::Line(wanted) = check {
            wanted.assert_in(
                &wir_lines(wir_text),
                "WIR line",
                test_id,
                &format!("WIR output:\n{wir_text}"),
            );
            continue;
        }
        assert!(
            check.holds(wir_text),
            "[{test_id}] {} failed\nWIR output:\n{wir_text}",
            check.describe(name)
        );
    }
}

/// Check that every WIR expectation under `level` is measured: it fails with
/// that level's `wir_pass` skipped, or on one of its `wir_contrast` twins. A
/// skip is tried with all the passes at once, then, for an expectation the
/// joint skip leaves standing, one of them alone: skipping one pass can remove
/// the shape another pass's expectation looks for. A twin no expectation fails
/// on contrasts nothing, and is reported as one. Every expectation that holds
/// anyway is reported at once.
fn assert_wir_checks_measured(
    compile: &dyn Fn(&[String]) -> Option<String>,
    compile_twin: &dyn Fn(&indexmap::IndexMap<String, String>) -> Option<String>,
    spec: &TestSpec,
    level: OptLevel,
    test_id: &str,
) {
    let name = common::opt_level_name(level);
    let passes = spec.wir_pass.get(name).map_or(&[][..], Vec::as_slice);
    let twins = spec.wir_contrast.get(name).map_or(&[][..], Vec::as_slice);
    assert!(
        !passes.is_empty() || !twins.is_empty(),
        "[{test_id}] the WIR expectations under `{name}` name no `wir_pass` and no \
         `wir_contrast`: list the passes whose work they are, which the run skips to see \
         each fail, or twins of the source each fails on"
    );
    let checks = spec.wir_checks(level);
    // A variant whose WIR holds no function `wir_scope` names is checked
    // against an empty body: a `wir_expect` fails there, and a
    // `wir_not_expect` holds without measuring anything.
    let holding = |wir: Option<String>, what: &str| -> (String, Vec<usize>) {
        let wir = wir.unwrap_or_else(|| panic!("[{test_id}] the compile {what} failed"));
        let body = match spec.wir_scope.as_deref() {
            None => wir.as_str(),
            Some(scope) => scope_to_function(&wir, scope).unwrap_or_default(),
        };
        let held = (0..checks.len())
            .filter(|&i| checks[i].holds(body))
            .collect();
        (body.to_string(), held)
    };
    let mut held: Vec<usize> = (0..checks.len()).collect();
    let mut last_wir = String::new();
    if !passes.is_empty() {
        let skipping = |skip: &[String]| holding(compile(skip), &format!("skipping {skip:?}"));
        (last_wir, held) = skipping(passes);
        if passes.len() > 1 {
            for pass in passes {
                if held.is_empty() {
                    break;
                }
                let (_, still) = skipping(std::slice::from_ref(pass));
                held.retain(|i| still.contains(i));
            }
        }
    }
    for edits in twins {
        let (wir, still) = holding(compile_twin(edits), &format!("of the twin {edits:?}"));
        assert!(
            still.len() < checks.len(),
            "[{test_id}] every WIR expectation under `{name}` holds on the twin {edits:?}, \
             so it contrasts nothing\nWIR of the twin:\n{wir}"
        );
        held.retain(|i| still.contains(i));
        last_wir = wir;
    }
    let held: Vec<String> = held.iter().map(|&i| checks[i].describe(name)).collect();
    assert!(
        held.is_empty(),
        "[{test_id}] these hold with {passes:?} skipped, jointly and one at a time, and on \
         every twin, so they measure nothing; one that says what the passes must leave \
         standing takes a twin under `wir_contrast` that makes them do it:\n  {}\n\
         WIR of the last variant:\n{last_wir}",
        held.join("\n  ")
    );
}

/// The fixture's source with `edits` applied, each `from` text occurring once
/// in the program (the `__DATA__` section, which states the edits, excluded).
fn twin_source(source: &str, edits: &indexmap::IndexMap<String, String>, test_id: &str) -> String {
    let split = source.find("\n__DATA__").unwrap_or(source.len());
    let (program, data) = source.split_at(split);
    let mut program = program.to_string();
    for (from, to) in edits {
        assert_eq!(
            program.matches(from.as_str()).count(),
            1,
            "[{test_id}] a `wir_contrast` edit names text the program holds once: {from:?}"
        );
        program = program.replacen(from.as_str(), to, 1);
    }
    program + data
}

/// Search mode: which expectations under `level` fail with each candidate pass
/// skipped alone, and how many WIR lines that skip changes; then, for each
/// expectation no single skip fails, a minimal set that does. Each result is a
/// JSON line appended to `out`.
fn search_wir_passes(
    out: &str,
    compile: &dyn Fn(&[String]) -> Option<String>,
    normal_wir: &str,
    spec: &TestSpec,
    level: OptLevel,
    test_id: &str,
) {
    use std::io::Write as _;
    let candidates: Vec<String> = std::env::var("WADO_E2E_WIR_PASS_CANDIDATES")
        .expect("WADO_E2E_WIR_PASS_CANDIDATES lists the passes to try")
        .split(',')
        .map(str::to_string)
        .collect();
    let checks = spec.wir_checks(level);
    let key = common::opt_level_name(level);
    // An absent function is an empty body, as `assert_wir_checks_measured`
    // reads one.
    let failing = |wir: &str| -> Vec<usize> {
        let body = match spec.wir_scope.as_deref() {
            None => wir,
            Some(scope) => match std::panic::catch_unwind(|| scope_to_function(wir, scope)) {
                // Ambiguous: the skip minted a second function of that name.
                Err(_) => return vec![],
                Ok(body) => body.unwrap_or_default(),
            },
        };
        (0..checks.len())
            .filter(|&i| !checks[i].holds(body))
            .collect()
    };
    let mut normal: BTreeMap<&str, i64> = BTreeMap::new();
    for line in normal_wir.lines() {
        *normal.entry(line).or_default() += 1;
    }
    let diff = |wir: &str| -> i64 {
        let mut counts = normal.clone();
        for line in wir.lines() {
            *counts.entry(line).or_default() -= 1;
        }
        counts.values().map(|n| n.abs()).sum()
    };
    let record = |skip: &[String], wir: &str| {
        serde_json::json!({
            "test": test_id, "key": key, "skip": skip,
            "fails": failing(wir), "diff": diff(wir),
        })
    };
    let mut lines = vec![serde_json::json!({
        "test": test_id, "key": key,
        "checks": checks.iter().map(|c| c.describe(key)).collect::<Vec<_>>(),
    })];
    let mut covered = vec![false; checks.len()];
    for pass in &candidates {
        let skip = [pass.clone()];
        let Some(wir) = compile(&skip) else {
            lines.push(serde_json::json!({"test": test_id, "key": key, "skip": skip, "compile_fail": true}));
            continue;
        };
        let line = record(&skip, &wir);
        for i in line["fails"].as_array().unwrap() {
            covered[i.as_u64().unwrap() as usize] = true;
        }
        if line["diff"].as_i64().unwrap() > 0 {
            lines.push(line);
        }
    }
    for i in (0..checks.len()).filter(|&i| !covered[i]) {
        let fails_i = |skip: &[String]| compile(skip).is_some_and(|wir| failing(&wir).contains(&i));
        if !fails_i(&candidates) {
            lines.push(serde_json::json!({"test": test_id, "key": key, "unresolved": i}));
            continue;
        }
        let set = ddmin(candidates.clone(), &fails_i);
        let wir = compile(&set).unwrap();
        lines.push(record(&set, &wir));
    }
    let text: String = lines.iter().map(|l| l.to_string() + "\n").collect();
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(out)
        .and_then(|mut f| f.write_all(text.as_bytes()))
        .unwrap();
}

/// A 1-minimal subset of `set` that `holds` still answers true for, by delta
/// debugging.
fn ddmin(mut set: Vec<String>, holds: &dyn Fn(&[String]) -> bool) -> Vec<String> {
    let mut n = 2;
    while set.len() >= 2 {
        let chunk = set.len().div_ceil(n);
        let chunks: Vec<Vec<String>> = set.chunks(chunk).map(<[String]>::to_vec).collect();
        if let Some(c) = chunks.iter().find(|c| holds(c)) {
            set.clone_from(c);
            n = 2;
            continue;
        }
        let rest = chunks.iter().find_map(|c| {
            let rest: Vec<String> = set.iter().filter(|p| !c.contains(p)).cloned().collect();
            (chunks.len() > 2 && holds(&rest)).then_some(rest)
        });
        if let Some(rest) = rest {
            set = rest;
            n = (n - 1).max(2);
            continue;
        }
        if n >= set.len() {
            break;
        }
        n = (2 * n).min(set.len());
    }
    set
}

/// Back each `preopened_dirs` entry with a fresh temp dir so filesystem tests
/// stay hermetic across the parallel per-optimization-level runs. An empty
/// `template` yields empty scratch; otherwise the directory it names, relative
/// to the fixture's own, is copied in as a seed corpus. Returns the
/// `(host, guest)` pairs plus the owning `TempDir` guards, which the caller
/// must keep alive until the guest finishes.
fn prepare_preopened_dirs(
    fixture_path: &Path,
    specs: &[[String; 2]],
    test_id: &str,
) -> (Vec<(String, String)>, Vec<tempfile::TempDir>) {
    let fixture_dir = fixture_path
        .parent()
        .expect("a fixture is a file in a directory");
    let mut dirs = Vec::with_capacity(specs.len());
    let mut temp_dirs = Vec::with_capacity(specs.len());

    for [template, guest_path] in specs {
        let temp = tempfile::tempdir()
            .unwrap_or_else(|e| panic!("[{test_id}] failed to create temp preopen dir: {e}"));

        if !template.is_empty() {
            copy_dir_recursive(&fixture_dir.join(template), temp.path()).unwrap_or_else(|e| {
                panic!("[{test_id}] failed to seed preopen dir from {template:?}: {e}")
            });
        }

        dirs.push((
            temp.path().to_string_lossy().into_owned(),
            guest_path.clone(),
        ));
        temp_dirs.push(temp);
    }

    (dirs, temp_dirs)
}

/// Recursively copy the contents of `src` into the existing directory `dst`.
fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            std::fs::create_dir(&to)?;
            copy_dir_recursive(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Harness entry points (one per optimization level)
// ---------------------------------------------------------------------------

/// Test function for O0 (no optimization)
/// `#[ignore]`d unless `CI` or `WADO_FULL_TEST` is set at build time (see the
/// `harness!` entry below). Run locally with `mise run test-e2e-level O0`.
fn fixture_test_o0(path: &Path, content: &str) -> Result<(), Box<dyn std::error::Error>> {
    run_fixture_test_with_opt(path, content, OptLevel::O0);
    Ok(())
}

/// Test function for O2 (full optimization)
fn fixture_test_o2(path: &Path, content: &str) -> Result<(), Box<dyn std::error::Error>> {
    run_fixture_test_with_opt(path, content, OptLevel::O2);
    Ok(())
}

/// Test function for O1 (development optimization)
/// `#[ignore]`d unless `CI` or `WADO_FULL_TEST` is set at build time (see the
/// `harness!` entry below). Run locally with `mise run test-e2e-level O1`.
fn fixture_test_o1(path: &Path, content: &str) -> Result<(), Box<dyn std::error::Error>> {
    run_fixture_test_with_opt(path, content, OptLevel::O1);
    Ok(())
}

/// Test function for O3 (aggressive optimization)
/// `#[ignore]`d unless `CI` or `WADO_FULL_TEST` is set at build time (see the
/// `harness!` entry below). Run locally with `mise run test-e2e-level O3`.
fn fixture_test_o3(path: &Path, content: &str) -> Result<(), Box<dyn std::error::Error>> {
    run_fixture_test_with_opt(path, content, OptLevel::O3);
    Ok(())
}

/// Test function for Os (size optimization)
/// `#[ignore]`d unless `CI` or `WADO_FULL_TEST` is set at build time (see the
/// `harness!` entry below). Run locally with `mise run test-e2e-level Os`.
fn fixture_test_os(path: &Path, content: &str) -> Result<(), Box<dyn std::error::Error>> {
    run_fixture_test_with_opt(path, content, OptLevel::Os);
    Ok(())
}

/// The diagnostics a failed assertion prints, each as `CODE: message`.
fn diagnostic_text(diagnostics: &[common::CapturedDiagnostic]) -> String {
    diagnostics.iter().fold(String::new(), |mut text, d| {
        let _ = write!(text, "\n    {}: {}", d.code, d.message);
        text
    })
}

/// Check each `wat_lines` entry against the emitted component's WAT.
fn assert_wat_lines(wasm: &[u8], spec: &TestSpec, test_id: &str) {
    if spec.wat_lines.is_empty() {
        return;
    }
    // `-Os` strips the symbol table, so a `$name` never appears there. Caught
    // at every level, the fixture fails where it is written rather than in the
    // one CI job that runs `-Os`.
    assert!(
        !spec.runs_at(OptLevel::Os)
            || !spec
                .wat_lines
                .iter()
                .any(|line| line.contains.iter().any(|needle| needle.contains('$'))),
        "[{test_id}] a wat_lines needle names a symbol (`$…`), which `-Os` strips; \
         set `skip_os` on this fixture, or keep `Os` out of its `only_opt`"
    );
    let wat = wasmprinter::print_bytes(wasm)
        .unwrap_or_else(|e| panic!("[{test_id}] disassembling the component failed: {e}"));
    let lines: Vec<&str> = wat.lines().map(str::trim).collect();
    for wanted in &spec.wat_lines {
        wanted.assert_in(&lines, "WAT line", test_id, &format!("full WAT:\n{wat}"));
    }
}

/// Write the compiled wasm (and a wat decoded from it) under `dir` so it can be
/// inspected and diffed against `wado compile`. Activated by setting the
/// `WADO_KEEP_WASM_DIR` environment variable. Failures are reported via
/// `eprintln!` and never block the test.
fn keep_wasm_artifacts(dir: &str, fixture_name: &str, opt_name: &str, wasm: &[u8], test_id: &str) {
    let dir_path = std::path::Path::new(dir);
    if let Err(e) = std::fs::create_dir_all(dir_path) {
        eprintln!("[{test_id}] WADO_KEEP_WASM_DIR: cannot create '{dir}': {e}");
        return;
    }
    let stem = fixture_name.strip_suffix(".wado").unwrap_or(fixture_name);
    let wasm_path = dir_path.join(format!("{stem}.{opt_name}.wasm"));
    if let Err(e) = std::fs::write(&wasm_path, wasm) {
        eprintln!(
            "[{test_id}] WADO_KEEP_WASM_DIR: cannot write {}: {e}",
            wasm_path.display()
        );
        return;
    }
    let wat_path = dir_path.join(format!("{stem}.{opt_name}.wat"));
    match wasmprinter::print_bytes(wasm) {
        Ok(text) => {
            if let Err(e) = std::fs::write(&wat_path, text) {
                eprintln!(
                    "[{test_id}] WADO_KEEP_WASM_DIR: cannot write {}: {e}",
                    wat_path.display()
                );
            }
        }
        Err(e) => {
            eprintln!("[{test_id}] WADO_KEEP_WASM_DIR: wasmprinter failed: {e}");
        }
    }
}

datatest_mini::harness! {
    // Pattern matches .wado files directly in fixtures/ but not in subdirectories
    // (subdirectories contain helper modules that are imported, not run as tests).
    //
    // Every level but O2 is `#[ignore]`d unless CI or WADO_FULL_TEST is set at
    // build time. The env is read at macro expansion, so toggling requires
    // re-expanding the macro (touch this file or `cargo clean`); locally, run
    // one on demand with `mise run test-e2e-level <LEVEL>`.
    { test = fixture_test_o0, root = "fixtures", pattern = r"^[^/]+\.wado$",
      ignore_unless_env = ["CI", "WADO_FULL_TEST"] },
    { test = fixture_test_o1, root = "fixtures", pattern = r"^[^/]+\.wado$",
      ignore_unless_env = ["CI", "WADO_FULL_TEST"] },
    { test = fixture_test_o2, root = "fixtures", pattern = r"^[^/]+\.wado$" },
    { test = fixture_test_o3, root = "fixtures", pattern = r"^[^/]+\.wado$",
      ignore_unless_env = ["CI", "WADO_FULL_TEST"] },
    { test = fixture_test_os, root = "fixtures", pattern = r"^[^/]+\.wado$",
      ignore_unless_env = ["CI", "WADO_FULL_TEST"] },
}

/// End-to-end check on the `#![TODO]` wrapper: a module-level `#![TODO]` whose
/// test passes must still hard-fail, and point at the module's mark.
#[test]
#[should_panic(
    expected = "TODO test 'test-todo-0-passes-unexpectedly' passed: its module is #![TODO]"
)]
fn module_todo_with_passing_test_hard_fails() {
    let source = r#"#![TODO]
test "passes unexpectedly" {
    assert true;
}

__DATA__
{"test": {}}
"#;
    run_fixture_test_with_opt(
        std::path::Path::new("synthetic_module_todo.wado"),
        source,
        OptLevel::O0,
    );
}

/// A `#[TODO]` test that returns Ok must surface as a hard failure rather than
/// being swallowed as "pending" by the outer `#![TODO]` `catch_unwind` path.
/// We exercise the runner end-to-end: compile a synthetic source whose only
/// test is `#[TODO]` and passes, then call `run_test_world` and assert that
/// the resulting panic payload carries the `TodoResolved` sentinel.
#[test]
fn run_test_world_signals_resolved_todo_via_sentinel() {
    let source = r#"#[TODO]
test "passes unexpectedly" {
    assert true;
}
"#;
    let wasm = compile_synthetic(source, Some("test"));

    let payload = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        common::run_test_world(
            &wasm,
            "synthetic-todo-resolved",
            indexmap::IndexMap::new(),
            indexmap::IndexMap::new(),
        )
    }))
    .expect_err("a resolved #[TODO] test must panic, not return");

    let resolved = payload
        .downcast_ref::<common::TodoResolved>()
        .expect("panic payload should be TodoResolved");
    assert!(
        resolved.report(TodoMark::Test).contains("remove #[TODO]"),
        "the report should say what to do: {}",
        resolved.0,
    );
    assert!(
        resolved.0.contains("passes-unexpectedly"),
        "message should mention the offending test name: {}",
        resolved.0,
    );
}

fn compile_synthetic(source: &str, target_world: Option<&str>) -> Vec<u8> {
    let options = CompilerOptions {
        opt_level: OptLevel::O0,
        target_world: target_world.map(str::to_string),
        ..Default::default()
    };
    let path = std::path::Path::new("synthetic.wado");
    common::compile_source_with_compiler_options(path, source, options)
        .expect("compilation should succeed")
        .wasm
}

/// A run that never ends spends its budget rather than trapping, so it cannot
/// satisfy a test that expects a trap.
#[test]
fn an_exhausted_budget_is_not_an_expected_trap() {
    let wasm = compile_synthetic(
        r#"#[expect_trap]
#[timeout_ms(100)]
test "never ends" {
    while builtin::black_box(true) {}
}
"#,
        Some("test"),
    );
    let error = common::run_test_world(
        &wasm,
        "synthetic-budget",
        indexmap::IndexMap::new(),
        indexmap::IndexMap::new(),
    )
    .expect_err("an exhausted budget must fail the test");
    assert!(error.is::<common::BudgetExhausted>(), "{error:#}");
}

#[test]
fn an_exhausted_budget_is_not_a_cli_trap() {
    let wasm = compile_synthetic(
        r#"export fn run() {
    while builtin::black_box(true) {}
}
"#,
        None,
    );
    let error = common::run_wasm(wasm).expect_err("an exhausted budget must not read as a trap");
    assert!(error.is::<common::BudgetExhausted>(), "{error:#}");
}
