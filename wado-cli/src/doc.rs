use std::fmt::Write as FmtWrite;
use std::fs;
use std::path::Path;

use lexopt::Arg::Value;
use wado_compiler::LogLevel;
use wado_compiler::doc::{
    DocEffect, DocEnum, DocFlags, DocFunction, DocImplMembers, DocModule, DocPrimitiveType,
    DocResource, DocScope, DocStruct, DocTrait, DocTraitImpl, DocVariant, extract_doc_with,
    extract_stdlib_doc_with, stdlib_doc_entry,
};
use wado_compiler::kiln::InvocationIndex;
use wado_compiler::semantics::{Semantics, semantics_for_world};
use wado_lsp::host::discovery::Disk;

use crate::args::{self, CliExit};
use crate::compile::{Settled, build_load, load_nearest_manifest};
use crate::compiler_host::FilesystemCompilerHost;
use crate::markdown::{FenceMarker, fence_open, matches_fence_close};
use crate::marl;
use crate::search::{self, Query};

const MODULE_PLACEHOLDER: &str = "{module}";

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    Markdown,
    Simple,
    Json,
}

pub struct DocOptions {
    pub inputs: Vec<String>,
    pub format: OutputFormat,
    pub title: Option<String>,
    /// Output destination. `None` writes to stdout. A path containing
    /// `{module}` enables batch mode (one file per input module).
    pub output: Option<String>,
    /// Document private items, fields, and inherent methods too.
    pub all: bool,
    /// List the items matching this query instead of rendering pages.
    pub search: Option<String>,
}

#[derive(Clone, Copy)]
enum Opt {
    Format,
    Title,
    Output,
    All,
    Search,
    Help,
}

impl Opt {
    const ALL: &[Self] = &[
        Self::Format,
        Self::Title,
        Self::Output,
        Self::All,
        Self::Search,
        Self::Help,
    ];

    const fn spec(self) -> args::OptSpec {
        match self {
            // No short form: `-f` is the codegen feature flag everywhere.
            Self::Format => args::OptSpec {
                long: Some("format"),
                short: None,
                value: Some("<fmt>"),
                desc: "Output format: markdown (default), simple, json",
            },
            Self::Title => args::OptSpec {
                long: Some("title"),
                short: None,
                value: Some("<title>"),
                desc: "Document title (required for multiple modules)",
            },
            Self::Output => args::OptSpec {
                long: Some("output"),
                short: Some('o'),
                value: Some("<path>"),
                desc: "Write to file instead of stdout. \
                       A `{module}` placeholder enables batch mode (one file per module).",
            },
            Self::All => args::OptSpec {
                long: Some("all"),
                short: None,
                value: None,
                desc: "Document private items, fields, and inherent methods too",
            },
            Self::Search => args::OptSpec {
                long: Some("search"),
                short: Some('s'),
                value: Some("<query>"),
                desc: "List the items holding every term of the query, in the inputs \
                       or, with none, in every stdlib module",
            },
            Self::Help => args::HELP_SPEC,
        }
    }
}

fn format_usage() -> String {
    let mut buf = String::new();
    writeln!(buf, "Usage: wado doc [options] <file.wado | module>...").unwrap();
    writeln!(buf).unwrap();
    writeln!(
        buf,
        "Generate documentation from Wado source files or stdlib modules."
    )
    .unwrap();
    writeln!(
        buf,
        "Accepts file paths or module names (e.g., core:cli, wasi:http)."
    )
    .unwrap();
    writeln!(
        buf,
        "Outputs to stdout by default; use `-o` to write to a file."
    )
    .unwrap();
    writeln!(
        buf,
        "Use `-o <path>` with a `{{module}}` placeholder to write one file per module."
    )
    .unwrap();
    writeln!(buf).unwrap();
    writeln!(buf, "Formats:").unwrap();
    writeln!(
        buf,
        "  markdown   Structured markdown with doc comments (default)"
    )
    .unwrap();
    writeln!(
        buf,
        "  simple     Compact pseudo-code for cheatsheet generation"
    )
    .unwrap();
    writeln!(buf, "  json       JSON document model (machine-readable)").unwrap();
    writeln!(buf).unwrap();
    writeln!(buf, "Options:").unwrap();
    write!(buf, "{}", args::format_opts_help(Opt::ALL, |o| o.spec())).unwrap();
    buf
}

pub fn parse_args(mut parser: lexopt::Parser) -> Result<DocOptions, CliExit> {
    let usage = format_usage();
    let mut inputs: Vec<String> = Vec::new();
    let mut format = OutputFormat::Markdown;
    let mut title: Option<String> = None;
    let mut output: Option<String> = None;
    let mut all = false;
    let mut search: Option<String> = None;
    let mut format_given = false;

    while let Some(arg) = args::next_arg(&mut parser)? {
        if let Some(opt) = args::match_opt(&arg, Opt::ALL, |o| o.spec()) {
            match opt {
                Opt::Format => {
                    format_given = true;
                    let fmt_str = args::require_string(&mut parser)?;
                    format = match fmt_str.as_str() {
                        "markdown" | "md" => OutputFormat::Markdown,
                        "simple" => OutputFormat::Simple,
                        "json" => OutputFormat::Json,
                        _ => {
                            return Err(CliExit::error(format!(
                                "unknown format '{fmt_str}'. Use markdown, simple, or json"
                            )));
                        }
                    };
                }
                Opt::Title => {
                    title = Some(args::require_string(&mut parser)?);
                }
                Opt::Output => {
                    output = Some(args::require_string(&mut parser)?);
                }
                Opt::All => all = true,
                Opt::Search => search = Some(args::require_string(&mut parser)?),
                Opt::Help => return Err(CliExit::help(usage)),
            }
        } else if let Value(val) = arg {
            inputs.push(val.to_string_lossy().into_owned());
        } else {
            return Err(args::unexpected_arg(arg, &usage));
        }
    }

    if search.is_some() {
        if format_given || title.is_some() || output.is_some() {
            return Err(CliExit::error(
                "--search prints a list of hits; it takes no --format, --title or --output",
            ));
        }
        return Ok(DocOptions {
            inputs,
            format,
            title,
            output,
            all,
            search,
        });
    }

    let inputs = args::require_inputs(inputs, &usage)?;
    let is_batch = output
        .as_deref()
        .is_some_and(|p| p.contains(MODULE_PLACEHOLDER));

    if is_batch && title.is_some() {
        return Err(CliExit::error(
            "--title cannot be combined with batch output (`{module}` template); \
             each file is rendered as a single module"
                .to_string(),
        ));
    }

    if !is_batch && inputs.len() > 1 && title.is_none() {
        return Err(CliExit::error(
            "--title is required when generating docs for multiple modules".to_string(),
        ));
    }

    Ok(DocOptions {
        inputs,
        format,
        title,
        output,
        all,
        search,
    })
}

fn is_stdlib_module(input: &str) -> bool {
    wado_compiler::module_source::is_bundled_specifier(input)
}

pub async fn run(opts: DocOptions) -> Result<(), CliExit> {
    if let Some(query) = &opts.search {
        let docs = if opts.inputs.is_empty() {
            load_stdlib_docs(opts.all).await?
        } else {
            let mut docs = Vec::with_capacity(opts.inputs.len());
            for input in &opts.inputs {
                docs.push((input.clone(), load_doc(input, opts.all).await?));
            }
            docs
        };
        print!("{}", search_docs(&docs, query)?);
        return Ok(());
    }

    let mut docs: Vec<(String, DocModule)> = Vec::with_capacity(opts.inputs.len());
    for input in &opts.inputs {
        docs.push((input.clone(), load_doc(input, opts.all).await?));
    }

    let format_name = match opts.format {
        OutputFormat::Markdown => "markdown",
        OutputFormat::Simple => "simple",
        OutputFormat::Json => "json",
    };

    let is_batch = opts
        .output
        .as_deref()
        .is_some_and(|p| p.contains(MODULE_PLACEHOLDER));

    if is_batch {
        let template = opts.output.as_deref().expect("batch mode requires output");
        for (input, doc) in &docs {
            let path = template.replace(MODULE_PLACEHOLDER, &module_filename_stem(input));
            let content = render_single(doc, format_name, input, opts.format);
            write_to_file(&path, &content)?;
        }
        return Ok(());
    }

    let combined = render_combined(&docs, opts.title.as_deref(), format_name, &opts);
    match opts.output.as_deref() {
        Some(path) => write_to_file(path, &combined)?,
        None => print!("{combined}"),
    }
    Ok(())
}

/// The host a page's analysis reports through: errors only, since a page is
/// written only of a program without them.
fn doc_host(dir: &Path) -> FilesystemCompilerHost {
    FilesystemCompilerHost::with_log_level(dir.to_path_buf(), LogLevel::Error)
}

/// The analysis of `source`, entered as `filename`, that a page reads to
/// resolve the names its module uses. Errors are reported, and the run fails.
/// Bodies are annotated but not reified: a page reads names, not TIR.
async fn analyze(
    source: &str,
    host: &FilesystemCompilerHost,
    filename: &str,
    invocations: InvocationIndex,
) -> Result<Semantics, CliExit> {
    let sem = semantics_for_world(source, host, Some(filename), None, invocations, false).await;
    if host.has_errors() {
        return Err(CliExit::silent_failure(1));
    }
    assert!(
        sem.is_complete(),
        "an analysis that reports no error completes"
    );
    Ok(sem)
}

/// The page of every documented stdlib module, from one analysis that loads
/// them all.
async fn load_stdlib_docs(include_private: bool) -> Result<Vec<(String, DocModule)>, CliExit> {
    let modules: Vec<&str> = wado_compiler::stdlib::documented_modules().collect();
    let entry: String = modules.iter().map(|m| stdlib_doc_entry(m)).collect();
    let host = doc_host(Path::new("."));
    let sem = analyze(&entry, &host, "doc.wado", InvocationIndex::new()).await?;
    Ok(modules
        .into_iter()
        .map(|m| {
            let doc = extract_stdlib_doc_with(m, include_private, &sem)
                .expect("an analyzed stdlib module is bundled");
            (m.to_string(), doc)
        })
        .collect())
}

/// One searchable item of a page: what a reader calls it, and what it says.
struct Item<'a> {
    module: &'a str,
    name: &'a str,
    /// The name under its owner's where it has one (`Option::map`).
    path: String,
    signature: &'a str,
    /// A function's attributes: `#[unavailable(…)]` says the name is reserved.
    attrs: &'a [String],
    doc: Option<&'a str>,
}

/// Every item of `doc`, in page order.
fn items<'a>(module: &'a str, doc: &'a DocModule) -> Vec<Item<'a>> {
    let mut out = Items {
        module,
        items: Vec::new(),
    };
    for g in &doc.globals {
        out.add(None, &g.name, &g.signature, &g.doc);
    }
    for f in &doc.functions {
        out.add_function(None, f);
    }
    for t in &doc.traits {
        out.add(None, &t.name, &t.signature, &t.doc);
        out.add_members(&t.name, &t.methods);
    }
    for e in &doc.effects {
        out.add(None, &e.name, &e.signature, &e.doc);
        out.add_members(&e.name, &e.methods);
    }
    for r in &doc.resources {
        out.add(None, &r.name, &r.signature, &r.doc);
        out.add_members(&r.name, r.operations.iter().chain(r.impls.functions()));
    }
    for t in &doc.types {
        out.add(None, &t.name, &t.signature, &t.doc);
        out.add_members(&t.name, t.impls.functions());
    }
    for p in &doc.primitive_types {
        out.add(None, &p.name, &p.name, &p.doc);
        out.add_members(&p.name, p.impls.functions());
    }
    for s in &doc.structs {
        out.add(None, &s.name, sig_name_part(&s.signature), &s.doc);
        for f in &s.fields {
            out.add(Some(&s.name), &f.name, &f.ty, &f.doc);
        }
        out.add_members(&s.name, s.impls.functions());
    }
    for v in &doc.variants {
        out.add(None, &v.name, &v.signature, &v.doc);
        for c in &v.cases {
            out.add(Some(&v.name), &c.name, &c.signature, &c.doc);
        }
        out.add_members(&v.name, v.impls.functions());
    }
    for e in &doc.enums {
        let signature = sig_name_part(&e.signature);
        out.add(None, &e.name, signature, &e.doc);
        for c in &e.cases {
            out.add(Some(&e.name), &c.name, signature, &c.doc);
        }
        out.add_members(&e.name, e.impls.functions());
    }
    for f in &doc.flags {
        out.add(None, &f.name, &f.signature, &f.doc);
        for m in &f.members {
            out.add(Some(&f.name), &m.name, &f.signature, &m.doc);
        }
        out.add_members(&f.name, f.impls.functions());
    }
    out.items
}

struct Items<'a> {
    module: &'a str,
    items: Vec<Item<'a>>,
}

impl<'a> Items<'a> {
    fn add(
        &mut self,
        owner: Option<&str>,
        name: &'a str,
        signature: &'a str,
        doc: &'a Option<String>,
    ) {
        self.push(owner, name, signature, &[], doc);
    }

    fn add_function(&mut self, owner: Option<&str>, f: &'a DocFunction) {
        self.push(owner, &f.name, &f.signature, &f.attrs, &f.doc);
    }

    fn push(
        &mut self,
        owner: Option<&str>,
        name: &'a str,
        signature: &'a str,
        attrs: &'a [String],
        doc: &'a Option<String>,
    ) {
        self.items.push(Item {
            module: self.module,
            name,
            path: owner.map_or_else(|| name.to_string(), |o| format!("{o}::{name}")),
            signature,
            attrs,
            doc: doc.as_deref(),
        });
    }

    fn add_members(&mut self, owner: &str, members: impl IntoIterator<Item = &'a DocFunction>) {
        for f in members {
            self.add_function(Some(owner), f);
        }
    }
}

fn search_docs(docs: &[(String, DocModule)], query: &str) -> Result<String, CliExit> {
    let query = Query::parse(query).ok_or_else(|| CliExit::error("the query is empty"))?;
    let mut hits = Vec::new();
    for (module, doc) in docs {
        for item in items(module, doc) {
            let fields = [
                (item.name, 10),
                (item.path.as_str(), 6),
                (item.signature, 3),
                (item.doc.unwrap_or(""), 1),
            ];
            if let Some(score) = query.score(&fields) {
                let exact = if query.is_exactly(item.name) { 10 } else { 0 };
                hits.push((score + exact, item));
            }
        }
    }
    let hits = search::rank(hits);
    if hits.is_empty() {
        return Err(CliExit::error(
            "no item matches; `wado spec --search` searches the language specification",
        ));
    }
    let mut out = String::new();
    for item in hits.iter().take(search::SHOWN_HITS) {
        writeln!(out, "{}  {}", item.module, item.path).unwrap();
        for attr in item.attrs {
            writeln!(out, "    {attr}").unwrap();
        }
        writeln!(out, "    {}", item.signature).unwrap();
    }
    if let Some(rest) = search::rest_line(hits.len()) {
        writeln!(out, "{rest}").unwrap();
    }
    Ok(out)
}

async fn load_doc(input: &str, include_private: bool) -> Result<DocModule, CliExit> {
    if is_stdlib_module(input) {
        let host = doc_host(Path::new("."));
        let entry = stdlib_doc_entry(input);
        let sem = analyze(&entry, &host, "doc.wado", InvocationIndex::new()).await?;
        return Ok(extract_stdlib_doc_with(input, include_private, &sem)
            .expect("an analyzed stdlib module is bundled"));
    }

    let path = Path::new(input);
    let source = fs::read_to_string(path)
        .map_err(|e| CliExit::error(format!("reading '{}': {e}", path.display())))?;
    let module_name = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .ok_or_else(|| CliExit::error(format!("not a file: '{}'", path.display())))?;
    let project = load_nearest_manifest(path, &Disk).map_err(CliExit::error)?;
    let dir = path.parent().unwrap_or(Path::new(""));
    let (host, invocations) = build_load(
        path,
        &source,
        project,
        doc_host(dir),
        false,
        "doc",
        &mut Settled::default(),
    )
    .await?;
    let sem = analyze(&source, &host, input, invocations).await?;

    let parsed = wado_compiler::parse(&source)
        .into_fail_fast()
        .expect("an analyzed source parses");
    Ok(extract_doc_with(
        &parsed.ast,
        &parsed.trivia,
        &source,
        module_name,
        include_private,
        &DocScope::of_entry(&sem),
    ))
}

/// Filesystem-safe stem for a module input: stdlib `core:cli` -> `core-cli`,
/// file paths -> file stem.
fn module_filename_stem(input: &str) -> String {
    if is_stdlib_module(input) {
        // A surviving `/` would have `write_to_file` create a directory.
        return input.replace([':', '/'], "-");
    }
    Path::new(input)
        .file_stem()
        .and_then(|s| s.to_str())
        .map_or_else(|| input.to_string(), str::to_string)
}

fn write_to_file(path: &str, content: &str) -> Result<(), CliExit> {
    if let Some(parent) = Path::new(path).parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)
            .map_err(|e| CliExit::error(format!("creating '{}': {e}", parent.display())))?;
    }
    fs::write(path, content).map_err(|e| CliExit::error(format!("writing '{path}': {e}")))?;
    Ok(())
}

fn render_single(doc: &DocModule, format_name: &str, input: &str, format: OutputFormat) -> String {
    match format {
        OutputFormat::Json => format!("{}\n", serde_json::to_string_pretty(doc).unwrap()),
        OutputFormat::Markdown | OutputFormat::Simple => {
            let mut out = String::new();
            render_auto_generated_header(&mut out, format_name, None, &[input.to_string()]);
            match format {
                OutputFormat::Markdown => out.push_str(&render_markdown(doc, 0)),
                OutputFormat::Simple => out.push_str(&render_simple(doc, 0)),
                OutputFormat::Json => unreachable!(),
            }
            format_markdown(&out)
        }
    }
}

fn render_combined(
    docs: &[(String, DocModule)],
    title: Option<&str>,
    format_name: &str,
    opts: &DocOptions,
) -> String {
    if matches!(opts.format, OutputFormat::Json) {
        return if docs.len() == 1 {
            format!("{}\n", serde_json::to_string_pretty(&docs[0].1).unwrap())
        } else {
            let modules: Vec<&DocModule> = docs.iter().map(|(_, d)| d).collect();
            format!("{}\n", serde_json::to_string_pretty(&modules).unwrap())
        };
    }

    let h_offset: usize = usize::from(title.is_some());
    let mut out = String::new();
    render_auto_generated_header(&mut out, format_name, title, &opts.inputs);
    if let Some(t) = title {
        writeln!(out, "# {t}\n").unwrap();
    }
    for (i, (_, doc)) in docs.iter().enumerate() {
        if i > 0 && !out.ends_with("\n\n") {
            if out.ends_with('\n') {
                out.push('\n');
            } else {
                out.push_str("\n\n");
            }
        }
        match opts.format {
            OutputFormat::Markdown => out.push_str(&render_markdown(doc, h_offset)),
            OutputFormat::Simple => out.push_str(&render_simple(doc, h_offset)),
            OutputFormat::Json => unreachable!(),
        }
    }
    format_markdown(&out)
}

/// `wado doc --format markdown` and `--format simple` emit what Marl formats,
/// so `mise run format-md` over the output is a no-op and downstream tooling
/// needs no post-format pass.
fn format_markdown(content: &str) -> String {
    marl::format(content)
}

#[cfg(test)]
mod format_contract_tests {
    use super::*;
    use futures::executor::block_on;
    use wado_lsp::host::install_dev_stdlib;
    use wado_lsp::test_support::in_memory_host;

    fn stdlib_doc_with(module: &str, include_private: bool) -> DocModule {
        install_dev_stdlib();
        block_on(load_doc(module, include_private))
            .unwrap_or_else(|_| panic!("{module} stdlib doc"))
    }

    fn stdlib_doc(module: &str) -> DocModule {
        stdlib_doc_with(module, false)
    }

    fn stdlib_doc_all(module: &str) -> DocModule {
        stdlib_doc_with(module, true)
    }

    #[test]
    fn all_view_includes_private_members() {
        // `append_bytes` is a non-`pub` inherent helper on CborDeserializer.
        let default = render_single(
            &stdlib_doc("core:cbor"),
            "simple",
            "core:cbor",
            OutputFormat::Simple,
        );
        let all = render_single(
            &stdlib_doc_all("core:cbor"),
            "simple",
            "core:cbor",
            OutputFormat::Simple,
        );
        assert!(
            !default.contains("fn append_bytes"),
            "default doc leaked a private method"
        );
        assert!(
            all.contains("fn append_bytes"),
            "--all doc must include private methods"
        );
    }

    fn assert_marl_stable(content: &str, label: &str) {
        let reformatted = format_markdown(content);
        assert_eq!(
            content, reformatted,
            "{label}: wado doc output must be Marl-stable but re-formatting changed it",
        );
    }

    #[test]
    fn markdown_output_is_marl_stable() {
        let doc = stdlib_doc("core:cli");
        let out = render_single(&doc, "markdown", "core:cli", OutputFormat::Markdown);
        assert_marl_stable(&out, "markdown single");
    }

    #[test]
    fn simple_output_is_marl_stable() {
        let doc = stdlib_doc("core:cli");
        let out = render_single(&doc, "simple", "core:cli", OutputFormat::Simple);
        assert_marl_stable(&out, "simple single");
    }

    fn markdown_of(source: &str) -> String {
        markdown_importing(source, &[])
    }

    fn simple_all_of(source: &str) -> String {
        let doc = page(source, &[], true);
        render_single(&doc, "simple", "demo.wado", OutputFormat::Simple)
    }

    fn markdown_importing(source: &str, deps: &[(&str, &str)]) -> String {
        let doc = page(source, deps, false);
        render_single(&doc, "markdown", "demo.wado", OutputFormat::Markdown)
    }

    /// The page of `source`, as `./demo.wado` beside the `(name, source)` files
    /// of `deps` it imports as `./name`.
    fn page(source: &str, deps: &[(&str, &str)], include_private: bool) -> DocModule {
        let paths: Vec<String> = deps.iter().map(|(name, _)| format!("./{name}")).collect();
        let mut files = vec![("./demo.wado", source)];
        files.extend(
            paths
                .iter()
                .map(String::as_str)
                .zip(deps.iter().map(|(_, s)| *s)),
        );
        let host = in_memory_host(&files);
        let sem = block_on(semantics_for_world(
            source,
            &host,
            Some("./demo.wado"),
            None,
            InvocationIndex::new(),
            false,
        ));
        assert!(
            !host.has_errors(),
            "a page's program has no errors: {:?}",
            host.diagnostics()
        );
        let parsed = wado_compiler::parse(source)
            .into_fail_fast()
            .expect("parse");
        extract_doc_with(
            &parsed.ast,
            &parsed.trivia,
            source,
            "demo",
            include_private,
            &DocScope::of_entry(&sem),
        )
    }

    /// A newtype lists the impls written for it, as a struct does.
    #[test]
    fn a_newtype_lists_its_impls() {
        let source = "//! Demo.\n\n/// Bytes.\npub type Bytes = List<u8>;\n\n\
                      impl Bytes {\n    /// The first byte.\n    pub fn head(&self) -> u8 { return self[0]; }\n}\n\n\
                      pub trait Sized2 with () {\n    fn size(&self) -> i32;\n}\n\n\
                      impl Sized2 for Bytes {\n    fn size(&self) -> i32 { return self.len(); }\n}\n";
        let md = markdown_of(source);
        assert!(md.contains("#### `pub fn head(&self) -> u8`"), "got:\n{md}");
        assert!(md.contains("#### `impl Sized2 for Bytes`"), "got:\n{md}");
        let simple = simple_all_of(source);
        assert!(
            simple.contains("impl Bytes {\n    pub fn head(&self) -> u8;"),
            "got:\n{simple}"
        );
        assert!(simple.contains("impl Sized2 for Bytes {"), "got:\n{simple}");
    }

    /// Every kind of type lists the impls written for it, as a struct does.
    #[test]
    fn a_variant_an_enum_and_flags_list_their_impls() {
        let source = "//! Demo.\n\n\
                      pub variant Shape {\n    Dot,\n    Disc(f64),\n}\n\n\
                      impl Shape {\n    /// The area.\n    pub fn area(&self) -> f64 { return 0.0; }\n}\n\n\
                      pub enum Hue {\n    Red,\n}\n\n\
                      impl Hue {\n    pub const FIRST: Hue = Hue::Red;\n}\n\n\
                      pub flags Mode {\n    Read,\n}\n\n\
                      impl Mode {\n    pub fn readable(&self) -> bool { return true; }\n}\n";
        let md = markdown_of(source);
        for heading in [
            "#### `pub fn area(&self) -> f64`",
            "#### `pub const FIRST: Hue`",
            "#### `pub fn readable(&self) -> bool`",
        ] {
            assert!(md.contains(heading), "missing {heading}:\n{md}");
        }
        let simple = simple_all_of(source);
        for block in ["impl Shape {", "impl Hue {", "impl Mode {"] {
            assert!(simple.contains(block), "missing {block}:\n{simple}");
        }
    }

    /// A resource lists its trait impls and constants beside its operations.
    #[test]
    fn a_resource_lists_its_impls() {
        let source = "//! Demo.\n\npub resource Conn {\n    fn close(&self);\n}\n\n\
                      impl Conn {\n    pub const LIMIT: i32 = 4;\n}\n\n\
                      pub trait Named with () {\n    fn label(&self) -> i32;\n}\n\n\
                      impl Named for Conn {\n    fn label(&self) -> i32 { return 0; }\n}\n";
        let md = markdown_of(source);
        for heading in ["#### `pub const LIMIT: i32`", "#### `impl Named for Conn`"] {
            assert!(md.contains(heading), "missing {heading}:\n{md}");
        }
        let simple = simple_all_of(source);
        assert!(simple.contains("impl Named for Conn {"), "{simple}");
    }

    /// A resource's operations and its `impl` methods are two keys of its JSON
    /// object: one key holding both would leave a parser one of them.
    #[test]
    fn a_resource_json_keeps_operations_and_methods_apart() {
        let source = "pub resource Conn {\n    fn close(&self);\n}\n\n\
                      impl Conn {\n    pub fn open() -> i32 { return 0; }\n}\n";
        let json = serde_json::to_string(&page(source, &[], false)).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        let conn = &value["resources"][0];
        assert_eq!(conn["operations"][0]["name"], "close", "{json}");
        assert_eq!(conn["methods"][0]["name"], "open", "{json}");
    }

    /// An associated type is listed as declared: its own parameters and its
    /// bounds are part of what an impl owes.
    #[test]
    fn an_associated_type_keeps_its_parameters_and_bounds() {
        let out = simple_all_of(
            "//! Demo.\n\npub trait Elem {}\n\npub trait Len {}\n\npub trait Store {\n    type Buf<E: Elem>: Len;\n    affine type Plain;\n}\n",
        );
        assert!(
            out.contains("    type Buf<E: Elem>: Len;\n    affine type Plain;\n"),
            "an associated type must be listed as declared:\n{out}"
        );
    }

    /// An impl owes each associated type as it owes each method, so the
    /// reference documents both, `affine` included.
    #[test]
    fn a_trait_documents_its_associated_types() {
        let out = markdown_of(
            "//! Demo.\n\npub trait Len {}\n\npub trait Store {\n    /// What it holds.\n    affine type Buf<E>: Len;\n}\n",
        );
        assert!(
            out.contains("#### `affine type Buf<E>: Len`\n\nWhat it holds.\n"),
            "an associated type must be documented:\n{out}"
        );
    }

    /// The `export(…)` targets parse ahead of the `fn`, yet the doc comment
    /// above them is the function's.
    #[test]
    fn an_export_mapping_keeps_the_doc_comment_of_its_function() {
        let out = markdown_of(
            "//! Demo.\n\nuse { Command } from \"wasi:cli\";\n\n/// The entry.\nexport(Command::run) fn main() {}\n",
        );
        assert!(
            out.contains("### `export(Command::run) fn main()`\n\nThe entry.\n"),
            "the doc comment must stay on `main`:\n{out}"
        );
    }

    /// `--all` documents a struct at every visibility.
    #[test]
    fn an_internal_struct_heads_its_impl_block_with_its_own_name() {
        let out = simple_all_of(
            "//! Demo.\n\ninternal struct Limit {\n    value: i32,\n}\n\nimpl Limit {\n    pub const MAX: i32 = 99;\n}\n",
        );
        assert!(
            out.contains("impl Limit {"),
            "an internal struct's impl block must name the struct:\n{out}"
        );
    }

    const GAUGE_WITH_A_PRIVATE_TRAIT: &str = "//! Demo.\n\ntrait Hidden {\n    fn hidden(&self) -> i32;\n}\n\npub trait Shown {\n    fn shown(&self) -> i32;\n}\n\npub struct Gauge {\n    value: i32,\n}\n\nimpl Hidden for Gauge {\n    fn hidden(&self) -> i32 {\n        return self.value;\n    }\n}\n\nimpl Shown for Gauge {\n    fn shown(&self) -> i32 {\n        return self.value;\n    }\n}\n";

    /// A reader who cannot name a trait cannot call its methods, so the page
    /// lists no impl of one.
    #[test]
    fn an_impl_of_a_private_trait_is_left_out() {
        let out = markdown_of(GAUGE_WITH_A_PRIVATE_TRAIT);
        assert!(
            out.contains("impl Shown for Gauge"),
            "an impl of a pub trait must be documented:\n{out}"
        );
        assert!(
            !out.contains("Hidden"),
            "an impl of a private trait must be left out:\n{out}"
        );
    }

    /// The trait and its impl reach the page from different modules, so the
    /// page follows the `use` to the trait's declaration, under any spelling.
    #[test]
    fn an_impl_of_an_imported_internal_trait_is_left_out() {
        let dep = "internal trait Hidden {\n    fn hidden(&self) -> i32;\n}\n\ninternal use { Hidden as Veiled } from \"./veiled.wado\";\n\npub trait Shown {\n    fn shown(&self) -> i32;\n}\n";
        let source = "//! Demo.\n\nuse { Hidden, Veiled as Masked, Shown as Visible } from \"./dep.wado\";\nuse dep from \"./dep.wado\";\n\npub struct Gauge {\n    value: i32,\n}\n\npub struct Meter {\n    value: i32,\n}\n\nimpl Hidden for Gauge {\n    fn hidden(&self) -> i32 {\n        return self.value;\n    }\n}\n\nimpl Masked for Gauge {\n    fn hidden(&self) -> i32 {\n        return self.value;\n    }\n}\n\nimpl dep::Hidden for Meter {\n    fn hidden(&self) -> i32 {\n        return self.value;\n    }\n}\n\nimpl Visible for Gauge {\n    fn shown(&self) -> i32 {\n        return self.value;\n    }\n}\n\nimpl dep::Shown for Meter {\n    fn shown(&self) -> i32 {\n        return self.value;\n    }\n}\n";
        let veiled = "internal trait Hidden {\n    fn hidden(&self) -> i32;\n}\n";
        let out = markdown_importing(source, &[("dep.wado", dep), ("veiled.wado", veiled)]);
        assert!(
            out.contains("impl Visible for Gauge") && out.contains("impl dep::Shown for Meter"),
            "an impl of an imported pub trait must be documented:\n{out}"
        );
        for hidden in ["Hidden", "Masked"] {
            assert!(
                !out.contains(hidden),
                "an impl of an internal trait must be left out:\n{out}"
            );
        }
    }

    #[test]
    fn all_view_includes_an_impl_of_a_private_trait() {
        let out = simple_all_of(GAUGE_WITH_A_PRIVATE_TRAIT);
        assert!(
            out.contains("impl Hidden for Gauge"),
            "--all documents every visibility:\n{out}"
        );
    }

    /// A `pub const` is part of the type's API wherever it is declared.
    #[test]
    fn a_struct_documents_its_associated_constants() {
        let out = markdown_of(
            "//! Demo.\n\npub struct Limit {\n    value: i32,\n}\n\nimpl Limit {\n    pub const MAX: i32 = 99;\n\n    pub fn get(&self) -> i32 {\n        return self.value;\n    }\n}\n",
        );
        assert!(
            out.contains("pub const MAX: i32"),
            "a struct's associated constant must be documented:\n{out}"
        );
    }

    /// A caller owes the contract, so the reference shows it.
    #[test]
    fn a_function_documents_its_contract() {
        let out = markdown_of(
            "//! Demo.\n\npub fn half(n: i32) -> i32\n    contract n % 2 == 0, \"n must be even\"\n{\n    return n / 2;\n}\n",
        );
        assert!(
            out.contains("`contract n % 2 == 0, \"n must be even\"`"),
            "a function's contract must be documented:\n{out}"
        );
    }

    /// A trait method's clauses live in a hidden holder, not on the method.
    #[test]
    fn a_trait_method_documents_its_contract() {
        let out = markdown_of(
            "//! Demo.\n\npub trait Pick with () {\n    fn size(&self) -> i32;\n\n    fn pick(&self, i: i32) -> i32\n        contract 0 <= i < self.size();\n}\n",
        );
        assert!(
            out.contains("`contract 0 <= i < self.size()`"),
            "a trait method's contract must be documented:\n{out}"
        );
    }

    /// A trait's clauses bind every impl of it, wherever the trait is declared.
    #[test]
    fn an_impl_method_documents_its_trait_methods_contract() {
        let dep = "pub trait Pick with () {\n    fn pick(&self, i: i32) -> i32\n        contract i >= 0;\n}\n";
        let source = "//! Demo.\n\nuse { Pick } from \"./dep.wado\";\nuse dep from \"./dep.wado\";\n\npub struct Deck {\n    top: i32,\n}\n\npub struct Hand {\n    top: i32,\n}\n\nimpl Pick for Deck {\n    fn pick(&self, i: i32) -> i32 {\n        return self.top + i;\n    }\n}\n\nimpl dep::Pick for Hand {\n    fn pick(&self, i: i32) -> i32 {\n        return self.top + i;\n    }\n}\n";
        let out = markdown_importing(source, &[("dep.wado", dep)]);
        assert_eq!(
            out.matches("`contract i >= 0`").count(),
            2,
            "an impl's method must document its trait method's contract:\n{out}"
        );
    }

    /// The trait's clause reads the impl's parameter names, so it names what
    /// the signature beside it declares.
    #[test]
    fn an_impl_method_reads_its_trait_methods_contract_under_its_own_names() {
        let source = "//! Demo.\n\npub trait Pick with () {\n    fn pick(&self, i: i32) -> i32\n        contract i >= 0;\n}\n\npub struct Deck {\n    top: i32,\n}\n\nimpl Pick for Deck {\n    fn pick(&self, j: i32) -> i32 {\n        let i = j;\n        return self.top + i;\n    }\n}\n";
        let out = markdown_of(source);
        assert!(
            out.contains("`contract i >= 0`") && out.contains("`contract j >= 0`"),
            "the trait reads its own name, the impl its own:\n{out}"
        );
    }

    #[test]
    fn a_type_parameter_renders_its_default() {
        let out = markdown_of(
            "//! Demo.\n\npub trait Combine<Rhs = Self> {\n    fn combine(&self, rhs: &Rhs) -> i32;\n}\n",
        );
        assert!(
            out.contains("trait Combine<Rhs = Self>"),
            "the default is what makes the argument optional:\n{out}"
        );
    }

    #[test]
    fn single_synopsis_renders_a_section() {
        let out = markdown_of(
            "//! Demo.\n\n#[synopsis]\ntest {\n    let x = 1;\n    assert x == 1;\n}\n",
        );
        assert!(out.contains("## Synopsis"), "missing heading:\n{out}");
        assert!(
            out.contains("```wado\nlet x = 1;\nassert x == 1;\n```"),
            "body not dedented verbatim:\n{out}"
        );
    }

    #[test]
    fn multiple_synopses_render_in_source_order() {
        let out = markdown_of(
            "#[synopsis]\ntest {\n    let a = 1;\n}\n\n#[synopsis]\ntest {\n    let b = 2;\n}\n",
        );
        assert_eq!(
            out.matches("## Synopsis").count(),
            1,
            "one heading expected:\n{out}"
        );
        let a = out.find("let a = 1;").expect("first body");
        let b = out.find("let b = 2;").expect("second body");
        assert!(a < b, "synopses must render in source order:\n{out}");
    }

    #[test]
    fn synopsis_with_expect_trap_and_todo_still_renders() {
        let out = markdown_of(
            "#[synopsis]\n#[expect_trap]\ntest {\n    panic(\"boom\");\n}\n\n#[synopsis]\n#[TODO]\ntest {\n    let y = 3;\n}\n",
        );
        assert!(
            out.contains("panic(\"boom\");"),
            "expect_trap synopsis missing:\n{out}"
        );
        assert!(out.contains("let y = 3;"), "TODO synopsis missing:\n{out}");
    }

    #[test]
    fn ordinary_test_produces_no_synopsis_section() {
        let out = markdown_of("test {\n    let x = 1;\n}\n");
        assert!(
            !out.contains("## Synopsis"),
            "plain test must not emit Synopsis:\n{out}"
        );
    }

    #[test]
    fn synopsis_output_is_marl_stable() {
        let out = markdown_of(
            "//! Demo.\n\n#[synopsis]\ntest {\n    let x = 1;\n    assert x == 1;\n}\n",
        );
        assert_marl_stable(&out, "markdown synopsis");
    }

    #[test]
    fn doc_text_keeps_a_lone_asterisk() {
        let out = markdown_of("//! Computes a*b+c over 24*60*60 seconds.\n");
        assert!(out.contains("a*b+c over 24*60*60"), "{out}");
    }

    #[test]
    fn combined_markdown_output_is_marl_stable() {
        let inputs = ["core:cli".to_string(), "core:base64".to_string()];
        let docs: Vec<(String, _)> = inputs.iter().map(|i| (i.clone(), stdlib_doc(i))).collect();
        let opts = DocOptions {
            inputs: inputs.to_vec(),
            format: OutputFormat::Markdown,
            title: Some("Test".to_string()),
            output: None,
            all: false,
            search: None,
        };
        let out = render_combined(&docs, opts.title.as_deref(), "markdown", &opts);
        assert_marl_stable(&out, "markdown combined");
    }

    #[test]
    fn search_puts_the_exact_name_first() {
        let docs = vec![("core:prelude".to_string(), stdlib_doc("core:prelude"))];
        let out = search_docs(&docs, "unwrap_or").unwrap();
        let first = out.lines().next().unwrap();
        assert!(
            first.starts_with("core:prelude  ") && first.ends_with("::unwrap_or"),
            "{out}"
        );
        assert!(
            out.contains("::unwrap_or_else\n    #[unavailable("),
            "a reserved name must say so:\n{out}"
        );
    }

    #[test]
    fn search_with_no_input_reaches_every_documented_module() {
        install_dev_stdlib();
        let docs = block_on(load_stdlib_docs(false)).unwrap();
        let names: Vec<&str> = docs.iter().map(|(m, _)| m.as_str()).collect();
        assert!(
            names.contains(&"core:json") && names.contains(&"wasi:http"),
            "{names:?}"
        );
        let out = search_docs(&docs, "from_string").unwrap();
        assert!(out.starts_with("core:json  from_string\n"), "{out}");
    }

    #[test]
    fn a_variant_case_hit_shows_its_payload() {
        let doc = page("pub variant Shape {\n    Disc(f64),\n}\n", &[], false);
        let out = search_docs(&[("demo.wado".to_string(), doc)], "disc").unwrap();
        assert_eq!(out, "demo.wado  Shape::Disc\n    Disc(f64)\n");
    }

    #[test]
    fn a_miss_points_at_spec_search() {
        let docs = vec![("core:cli".to_string(), stdlib_doc("core:cli"))];
        let err = search_docs(&docs, "zzzz").unwrap_err();
        assert!(
            err.message.contains("wado spec --search"),
            "{}",
            err.message
        );
    }

    /// `mise run doc-stdlib` names its modules by hand; each documented core
    /// module must have the page that task writes, and no page outlive one.
    #[test]
    fn every_documented_core_module_has_its_page() {
        let docs_dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../docs");
        let mut pages: Vec<String> = fs::read_dir(docs_dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|f| f.starts_with("stdlib-core-"))
            .collect();
        pages.sort();
        let mut expected: Vec<String> = wado_compiler::stdlib::documented_modules()
            .filter(|m| m.starts_with("core:"))
            .map(|m| format!("stdlib-{}.md", module_filename_stem(m)))
            .collect();
        expected.sort();
        assert_eq!(expected, pages);
    }
}

fn render_auto_generated_header(
    out: &mut String,
    format_name: &str,
    title: Option<&str>,
    inputs: &[String],
) {
    write!(
        out,
        "<!-- Auto-generated by: wado doc --format {format_name}"
    )
    .unwrap();
    if let Some(t) = title {
        write!(out, " --title \"{t}\"").unwrap();
    }
    for input in inputs {
        write!(out, " {input}").unwrap();
    }
    writeln!(out, " -->").unwrap();
    writeln!(out, "<!-- Do not edit this file directly. -->").unwrap();
    out.push('\n');
}

fn h(level: usize) -> &'static str {
    match level {
        1 => "#",
        2 => "##",
        3 => "###",
        4 => "####",
        5 => "#####",
        _ => "######",
    }
}

fn render_markdown(doc: &DocModule, h_offset: usize) -> String {
    let h1 = h(1 + h_offset);
    let h2 = h(2 + h_offset);
    let h3 = h(3 + h_offset);
    let h4 = h(4 + h_offset);

    let mut out = format!("{h1} {}\n", doc.name);

    if let Some(ref module_doc) = doc.doc {
        out.push('\n');
        render_md_doc(&mut out, module_doc);
    }

    render_md_synopsis_section(&mut out, doc, h2);
    render_md_globals_section(&mut out, doc, h2, h3);
    render_md_functions_section(&mut out, doc, h2, h3);
    render_md_traits_section(&mut out, doc, h2, h3, h4);
    render_md_effects_section(&mut out, doc, h2, h3, h4);
    render_md_resources_section(&mut out, doc, h2, h3, h4);
    render_md_types_section(&mut out, doc, h2, h3);
    render_md_primitive_types_section(&mut out, doc, h2, h3, h4);
    render_md_structs_section(&mut out, doc, h2, h3, h4);
    render_md_variants_section(&mut out, doc, h2, h3, h4);
    render_md_enums_section(&mut out, doc, h2, h3, h4);
    render_md_flags_section(&mut out, doc, h2, h3, h4);

    out
}

fn render_md_synopsis_section(out: &mut String, doc: &DocModule, h2: &str) {
    if doc.synopsis.is_empty() {
        return;
    }
    writeln!(out, "\n{h2} Synopsis").unwrap();
    for body in &doc.synopsis {
        writeln!(out, "\n```wado\n{body}\n```").unwrap();
    }
}

fn render_md_types_section(out: &mut String, doc: &DocModule, h2: &str, h3: &str) {
    if doc.types.is_empty() {
        return;
    }
    writeln!(out, "\n{h2} Types").unwrap();
    let h4 = h(heading_level(h3) + 1);
    for t in &doc.types {
        writeln!(out, "\n{h3} `{}`", t.signature).unwrap();
        if let Some(ref d) = t.doc {
            out.push('\n');
            render_md_doc(out, d);
        }
        render_md_impl_members(out, &t.impls, h4);
    }
}

/// The constants, methods and trait impls a type's `impl` blocks declare.
fn render_md_impl_members(out: &mut String, impls: &DocImplMembers, h4: &str) {
    for m in impls.inherent() {
        render_md_function(out, m, h4);
    }
    for ti in &impls.trait_impls {
        writeln!(out, "\n{h4} `{}`", ti.signature).unwrap();
        let h5 = h(heading_level(h4) + 1);
        for m in &ti.methods {
            render_md_function(out, m, h5);
        }
    }
}

fn render_md_globals_section(out: &mut String, doc: &DocModule, h2: &str, h3: &str) {
    if doc.globals.is_empty() {
        return;
    }
    writeln!(out, "\n{h2} Globals").unwrap();
    for g in &doc.globals {
        writeln!(out, "\n{h3} `{}`", g.signature).unwrap();
        if let Some(ref d) = g.doc {
            out.push('\n');
            render_md_doc(out, d);
        }
    }
}

fn render_md_flags_section(out: &mut String, doc: &DocModule, h2: &str, h3: &str, h4: &str) {
    if doc.flags.is_empty() {
        return;
    }
    writeln!(out, "\n{h2} Flags").unwrap();
    for f in &doc.flags {
        render_md_flags(out, f, h3, h4);
    }
}

fn render_md_enums_section(out: &mut String, doc: &DocModule, h2: &str, h3: &str, h4: &str) {
    if doc.enums.is_empty() {
        return;
    }
    writeln!(out, "\n{h2} Enums").unwrap();
    for e in &doc.enums {
        render_md_enum(out, e, h3, h4);
    }
}

fn render_md_effects_section(out: &mut String, doc: &DocModule, h2: &str, h3: &str, h4: &str) {
    if doc.effects.is_empty() {
        return;
    }
    writeln!(out, "\n{h2} Effects").unwrap();
    for e in &doc.effects {
        render_md_effect(out, e, h3, h4);
    }
}

fn render_md_resources_section(out: &mut String, doc: &DocModule, h2: &str, h3: &str, h4: &str) {
    if doc.resources.is_empty() {
        return;
    }
    writeln!(out, "\n{h2} Resources").unwrap();
    for r in &doc.resources {
        render_md_resource(out, r, h3, h4);
    }
}

fn render_md_traits_section(out: &mut String, doc: &DocModule, h2: &str, h3: &str, h4: &str) {
    if doc.traits.is_empty() {
        return;
    }
    writeln!(out, "\n{h2} Traits").unwrap();
    for t in &doc.traits {
        render_md_trait(out, t, h3, h4);
    }
}

fn render_md_structs_section(out: &mut String, doc: &DocModule, h2: &str, h3: &str, h4: &str) {
    if doc.structs.is_empty() {
        return;
    }
    writeln!(out, "\n{h2} Structs").unwrap();
    for s in &doc.structs {
        render_md_struct(out, s, h3, h4);
    }
}

fn render_md_primitive_types_section(
    out: &mut String,
    doc: &DocModule,
    h2: &str,
    h3: &str,
    h4: &str,
) {
    if doc.primitive_types.is_empty() {
        return;
    }
    writeln!(out, "\n{h2} Primitive Types").unwrap();
    for p in &doc.primitive_types {
        render_md_primitive_impl(out, p, h3, h4);
    }
}

fn render_md_primitive_impl(out: &mut String, p: &DocPrimitiveType, h3: &str, h4: &str) {
    writeln!(out, "\n{h3} `{}`", p.name).unwrap();
    if let Some(ref d) = p.doc {
        out.push('\n');
        render_md_doc(out, d);
    }
    render_md_impl_members(out, &p.impls, h4);
}

fn render_md_variants_section(out: &mut String, doc: &DocModule, h2: &str, h3: &str, h4: &str) {
    if doc.variants.is_empty() {
        return;
    }
    writeln!(out, "\n{h2} Variants").unwrap();
    for v in &doc.variants {
        render_md_variant(out, v, h3, h4);
    }
}

fn render_md_functions_section(out: &mut String, doc: &DocModule, h2: &str, h3: &str) {
    if doc.functions.is_empty() {
        return;
    }
    writeln!(out, "\n{h2} Functions").unwrap();
    for f in &doc.functions {
        render_md_function(out, f, h3);
    }
}

fn render_md_trait(out: &mut String, t: &DocTrait, h3: &str, h4: &str) {
    writeln!(out, "\n{h3} `{}`", t.signature).unwrap();
    if let Some(ref d) = t.doc {
        out.push('\n');
        render_md_doc(out, d);
    }
    for assoc in &t.associated_types {
        render_md_entity(out, &assoc.signature, assoc.doc.as_deref(), h4);
    }
    for m in &t.methods {
        render_md_function(out, m, h4);
    }
}

fn render_md_struct(out: &mut String, s: &DocStruct, h3: &str, h4: &str) {
    let name = sig_name_part(&s.signature);
    writeln!(out, "\n{h3} `{name}`").unwrap();
    if let Some(ref d) = s.doc {
        out.push('\n');
        render_md_doc(out, d);
    }
    if s.fields.is_empty() && s.has_private_fields {
        writeln!(out, "\n_Fields are private._").unwrap();
    }
    for f in &s.fields {
        render_md_entity(out, &format!("{}: {}", f.name, f.ty), f.doc.as_deref(), h4);
    }
    render_md_impl_members(out, &s.impls, h4);
}

fn render_md_enum(out: &mut String, e: &DocEnum, h3: &str, h4: &str) {
    let name = sig_name_part(&e.signature);
    writeln!(out, "\n{h3} `{name}`").unwrap();
    if let Some(ref d) = e.doc {
        out.push('\n');
        render_md_doc(out, d);
    }
    for case in &e.cases {
        render_md_entity(out, &case.name, case.doc.as_deref(), h4);
    }
    render_md_impl_members(out, &e.impls, h4);
}

fn render_md_variant(out: &mut String, v: &DocVariant, h3: &str, h4: &str) {
    writeln!(out, "\n{h3} `{}`", v.signature).unwrap();
    if let Some(ref d) = v.doc {
        out.push('\n');
        render_md_doc(out, d);
    }
    for case in &v.cases {
        render_md_entity(out, &case.signature, case.doc.as_deref(), h4);
    }
    render_md_impl_members(out, &v.impls, h4);
}

fn render_md_flags(out: &mut String, f: &DocFlags, h3: &str, h4: &str) {
    writeln!(out, "\n{h3} `{}`", f.signature).unwrap();
    if let Some(ref d) = f.doc {
        out.push('\n');
        render_md_doc(out, d);
    }
    for member in &f.members {
        render_md_entity(out, &member.name, member.doc.as_deref(), h4);
    }
    render_md_impl_members(out, &f.impls, h4);
}

fn render_md_effect(out: &mut String, e: &DocEffect, h3: &str, h4: &str) {
    writeln!(out, "\n{h3} `{}`", e.signature).unwrap();
    if let Some(ref d) = e.doc {
        out.push('\n');
        render_md_doc(out, d);
    }
    for m in &e.methods {
        render_md_function(out, m, h4);
    }
}

fn render_md_resource(out: &mut String, r: &DocResource, h3: &str, h4: &str) {
    writeln!(out, "\n{h3} `{}`", r.signature).unwrap();
    if let Some(ref d) = r.doc {
        out.push('\n');
        render_md_doc(out, d);
    }
    for m in &r.operations {
        render_md_function(out, m, h4);
    }
    render_md_impl_members(out, &r.impls, h4);
}

fn render_md_entity(out: &mut String, sig: &str, doc: Option<&str>, h: &str) {
    render_md_member(out, sig, &[], &[], doc, h);
}

fn render_md_function(out: &mut String, f: &DocFunction, h: &str) {
    render_md_member(
        out,
        &f.signature,
        &f.attrs,
        &f.contracts,
        f.doc.as_deref(),
        h,
    );
}

/// `text` as a code span. Text holding a backtick would close a plain span
/// early, so the delimiter outruns the longest run inside and pads the ends.
fn code_span(text: &str) -> String {
    let mut longest = 0;
    let mut run = 0;
    for c in text.chars() {
        run = if c == '`' { run + 1 } else { 0 };
        longest = longest.max(run);
    }
    let fence = "`".repeat(longest + 1);
    let pad = if text.starts_with('`') || text.ends_with('`') {
        " "
    } else {
        ""
    };
    format!("{fence}{pad}{text}{pad}{fence}")
}

/// A member's heading, the attributes its declaration carries, its `contract`
/// clauses, and its doc. The heading is one line, so the attributes go on a
/// line of their own under it, and the clauses in a list, one item each.
fn render_md_member(
    out: &mut String,
    sig: &str,
    attrs: &[String],
    contracts: &[String],
    doc: Option<&str>,
    h: &str,
) {
    writeln!(out, "\n{h} {}", code_span(sig)).unwrap();
    if !attrs.is_empty() {
        let rendered: Vec<String> = attrs.iter().map(|a| code_span(a)).collect();
        writeln!(out, "\n{}", rendered.join(" ")).unwrap();
    }
    if !contracts.is_empty() {
        out.push('\n');
        for clause in contracts {
            writeln!(out, "- {}", code_span(clause)).unwrap();
        }
    }
    if let Some(d) = doc {
        out.push('\n');
        render_md_doc(out, d);
    }
}

fn heading_level(h: &str) -> usize {
    h.chars().take_while(|&c| c == '#').count()
}

fn sig_name_part(sig: &str) -> &str {
    sig.find(" { ").map_or(sig, |i| &sig[..i])
}

fn render_md_doc(out: &mut String, doc: &str) {
    let mut prev_blank = true;
    let mut in_list = false;
    let mut in_blockquote = false;
    let mut prev_heading = false;
    let mut fence: Option<FenceMarker> = None;

    for line in doc.lines() {
        // Inside a fenced code block: detect the closing fence and emit
        // every other line verbatim. CommonMark heading / list / emphasis
        // rules do not apply inside the block.
        if let Some(open) = fence {
            let trimmed_start = line.trim_start();
            if matches_fence_close(trimmed_start, &open) {
                out.push_str(trimmed_start);
                out.push('\n');
                fence = None;
                prev_blank = false;
                prev_heading = false;
                in_list = false;
                in_blockquote = false;
                continue;
            }
            out.push_str(line);
            out.push('\n');
            prev_blank = false;
            continue;
        }

        // Detect an opening fence before any other markdown processing.
        if let Some(open) = fence_open(line.trim_start()) {
            if !prev_blank {
                out.push('\n');
            }
            out.push_str(line.trim_start());
            out.push('\n');
            fence = Some(open);
            prev_blank = false;
            prev_heading = false;
            in_list = false;
            in_blockquote = false;
            continue;
        }

        let normalized = normalize_md_line(line);
        let trimmed = normalized.trim();

        if trimmed.is_empty() {
            if !prev_blank {
                out.push('\n');
                prev_blank = true;
            }
            in_list = false;
            in_blockquote = false;
            prev_heading = false;
            continue;
        }

        let is_list_item = trimmed.starts_with("- ") || is_ordered_list_item(trimmed);
        let blockquote_line = trimmed.starts_with("> ");
        let is_heading = trimmed.starts_with('#');

        // Insert blank line before block elements or after headings
        if !prev_blank
            && ((is_list_item && !in_list) || (blockquote_line && !in_blockquote) || prev_heading)
        {
            out.push('\n');
        }

        // Write line with appropriate continuation prefix
        if in_blockquote && !blockquote_line && !is_heading {
            out.push_str("> ");
            out.push_str(&normalized);
        } else if in_list && !is_list_item && !is_heading && !blockquote_line {
            out.push_str("  ");
            out.push_str(&normalized);
        } else {
            out.push_str(&normalized);
        }
        out.push('\n');

        prev_blank = false;
        prev_heading = is_heading;
        if is_list_item {
            in_list = true;
            in_blockquote = false;
        } else if blockquote_line {
            in_blockquote = true;
            in_list = false;
        } else if is_heading {
            in_list = false;
            in_blockquote = false;
        }
    }
}

fn normalize_md_line(line: &str) -> String {
    let stripped = line.trim_start();
    collapse_md_spaces(stripped)
}

/// Collapse multiple consecutive spaces to single space, preserving code spans.
fn collapse_md_spaces(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut in_code = false;
    let mut prev_space = false;

    for c in s.chars() {
        if c == '`' {
            in_code.toggle();
            result.push(c);
            prev_space = false;
        } else if c == ' ' && !in_code {
            if !prev_space {
                result.push(' ');
            }
            prev_space = true;
        } else {
            result.push(c);
            prev_space = false;
        }
    }

    result
}

fn is_ordered_list_item(s: &str) -> bool {
    let rest = s.trim_start_matches(|c: char| c.is_ascii_digit());
    rest != s && rest.starts_with(". ")
}

fn render_simple(doc: &DocModule, h_offset: usize) -> String {
    let h1 = h(1 + h_offset);
    let h2 = h(2 + h_offset);

    let mut out = format!("{h1} {}\n", doc.name);

    if let Some(ref module_doc) = doc.doc {
        out.push('\n');
        out.push_str(module_doc);
        out.push('\n');
    }

    render_md_synopsis_section(&mut out, doc, h2);
    render_simple_globals_section(&mut out, doc, h2);
    render_simple_functions_section(&mut out, doc, h2);
    render_simple_traits_section(&mut out, doc, h2);
    render_simple_effects_section(&mut out, doc, h2);
    render_simple_resources_section(&mut out, doc, h2);
    render_simple_types_section(&mut out, doc, h2);
    render_simple_primitive_types_section(&mut out, doc, h2);
    render_simple_structs_section(&mut out, doc, h2);
    render_simple_variants_section(&mut out, doc, h2);
    render_simple_enums_section(&mut out, doc, h2);
    render_simple_flags_section(&mut out, doc, h2);

    out
}

fn render_simple_types_section(out: &mut String, doc: &DocModule, h2: &str) {
    if doc.types.is_empty() {
        return;
    }
    writeln!(out, "\n{h2} Types\n\n```wado").unwrap();
    for t in &doc.types {
        writeln!(out, "{};", t.signature).unwrap();
        render_simple_impls(out, &t.name, &t.impls);
    }
    out.push_str("```\n");
}

/// A type's `impl` blocks: the inherent one, where it declares anything, then
/// each trait impl.
fn render_simple_impls(out: &mut String, name: &str, impls: &DocImplMembers) {
    if impls.inherent().next().is_some() {
        out.push('\n');
        render_simple_impl_block(out, &format!("impl {name}"), impls.inherent());
    }
    render_simple_trait_impls(out, &impls.trait_impls);
}

fn render_simple_trait_impls(out: &mut String, trait_impls: &[DocTraitImpl]) {
    for ti in trait_impls {
        out.push('\n');
        render_simple_impl_block(out, &ti.signature, &ti.methods);
    }
}

fn render_simple_impl_block<'a>(
    out: &mut String,
    head: &str,
    members: impl IntoIterator<Item = &'a DocFunction>,
) {
    writeln!(out, "{head} {{").unwrap();
    for m in members {
        write_simple_decl(out, m, "    ");
    }
    out.push_str("}\n");
}

fn render_simple_globals_section(out: &mut String, doc: &DocModule, h2: &str) {
    if doc.globals.is_empty() {
        return;
    }
    writeln!(out, "\n{h2} Globals\n\n```wado").unwrap();
    for g in &doc.globals {
        writeln!(out, "{};", g.signature).unwrap();
    }
    out.push_str("```\n");
}

fn render_simple_flags_section(out: &mut String, doc: &DocModule, h2: &str) {
    if doc.flags.is_empty() {
        return;
    }
    writeln!(out, "\n{h2} Flags").unwrap();
    for f in &doc.flags {
        writeln!(out, "\n```wado\n{} {{", f.signature).unwrap();
        for member in &f.members {
            writeln!(out, "    {},", member.name).unwrap();
        }
        out.push_str("}\n");
        render_simple_impls(out, &f.name, &f.impls);
        out.push_str("```\n");
    }
}

fn render_simple_enums_section(out: &mut String, doc: &DocModule, h2: &str) {
    if doc.enums.is_empty() {
        return;
    }
    writeln!(out, "\n{h2} Enums").unwrap();
    for e in &doc.enums {
        writeln!(out, "\n```wado\n{} {{", sig_name_part(&e.signature)).unwrap();
        for case in &e.cases {
            writeln!(out, "    {},", case.name).unwrap();
        }
        out.push_str("}\n");
        render_simple_impls(out, &e.name, &e.impls);
        out.push_str("```\n");
    }
}

fn render_simple_effects_section(out: &mut String, doc: &DocModule, h2: &str) {
    if doc.effects.is_empty() {
        return;
    }
    writeln!(out, "\n{h2} Effects").unwrap();
    for e in &doc.effects {
        writeln!(out, "\n```wado\n{} {{", e.signature).unwrap();
        for m in &e.methods {
            write_simple_decl(out, m, "    ");
        }
        out.push_str("}\n```\n");
    }
}

fn render_simple_resources_section(out: &mut String, doc: &DocModule, h2: &str) {
    if doc.resources.is_empty() {
        return;
    }
    writeln!(out, "\n{h2} Resources").unwrap();
    for r in &doc.resources {
        out.push_str("\n```wado\n");
        if r.operations.is_empty() {
            writeln!(out, "{};", r.signature).unwrap();
        } else {
            writeln!(out, "{} {{", r.signature).unwrap();
            for m in &r.operations {
                write_simple_decl(out, m, "    ");
            }
            out.push_str("}\n");
        }
        render_simple_impls(out, &r.name, &r.impls);
        out.push_str("```\n");
    }
}

fn render_simple_traits_section(out: &mut String, doc: &DocModule, h2: &str) {
    if doc.traits.is_empty() {
        return;
    }
    writeln!(out, "\n{h2} Traits\n\n```wado").unwrap();
    for (i, t) in doc.traits.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        writeln!(out, "{} {{", t.signature).unwrap();
        for assoc in &t.associated_types {
            writeln!(out, "    {};", assoc.signature).unwrap();
        }
        for m in &t.methods {
            write_simple_decl(out, m, "    ");
        }
        out.push_str("}\n");
    }
    out.push_str("```\n");
}

fn render_simple_structs_section(out: &mut String, doc: &DocModule, h2: &str) {
    if doc.structs.is_empty() {
        return;
    }
    writeln!(out, "\n{h2} Structs").unwrap();
    for s in &doc.structs {
        out.push_str("\n```wado\n");
        render_simple_struct(out, s);
        render_simple_impls(out, &s.name, &s.impls);
        out.push_str("```\n");
    }
}

fn render_simple_primitive_types_section(out: &mut String, doc: &DocModule, h2: &str) {
    if doc.primitive_types.is_empty() {
        return;
    }
    writeln!(out, "\n{h2} Primitive Types").unwrap();
    for p in &doc.primitive_types {
        out.push_str("\n```wado\n");
        render_simple_impl_block(out, &format!("impl {}", p.name), p.impls.inherent());
        render_simple_trait_impls(out, &p.impls.trait_impls);
        out.push_str("```\n");
    }
}

fn render_simple_variants_section(out: &mut String, doc: &DocModule, h2: &str) {
    if doc.variants.is_empty() {
        return;
    }
    writeln!(out, "\n{h2} Variants").unwrap();
    for v in &doc.variants {
        writeln!(out, "\n```wado\n{} {{", v.signature).unwrap();
        for case in &v.cases {
            writeln!(out, "    {},", case.signature).unwrap();
        }
        out.push_str("}\n");
        render_simple_impls(out, &v.name, &v.impls);
        out.push_str("```\n");
    }
}

fn render_simple_functions_section(out: &mut String, doc: &DocModule, h2: &str) {
    if doc.functions.is_empty() {
        return;
    }
    writeln!(out, "\n{h2} Functions\n\n```wado").unwrap();
    for f in &doc.functions {
        write_simple_decl(out, f, "");
    }
    out.push_str("```\n");
}

/// A function declaration as source: its signature, then each `contract`
/// clause on a line of its own, as the formatter writes them.
fn write_simple_decl(out: &mut String, f: &DocFunction, indent: &str) {
    write!(out, "{indent}{}", f.signature).unwrap();
    for clause in &f.contracts {
        write!(out, "\n{indent}    {clause}").unwrap();
    }
    out.push_str(";\n");
}

/// Render a struct in simple format with fields on separate lines.
fn render_simple_struct(out: &mut String, s: &DocStruct) {
    // Extract the prefix: "pub struct Name<T>" (everything before " { ")
    let prefix = s
        .signature
        .find(" { ")
        .map_or(s.signature.as_str(), |i| &s.signature[..i]);

    if s.fields.is_empty() && !s.has_private_fields {
        // No fields at all: `pub struct Foo {}`
        writeln!(out, "{prefix} {{}}").unwrap();
    } else {
        writeln!(out, "{prefix} {{").unwrap();
        for f in &s.fields {
            writeln!(out, "    {}: {},", f.name, f.ty).unwrap();
        }
        if s.has_private_fields {
            out.push_str("    ..\n");
        }
        out.push_str("}\n");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(input: &str) -> String {
        let mut out = String::new();
        render_md_doc(&mut out, input);
        out
    }

    /// `{module}` fills a filename, so the stem must be one path component.
    #[test]
    fn a_bundled_module_stem_is_a_single_path_component() {
        for input in ["wasi:http/types", "core:prelude", "core:kiln/types"] {
            let stem = module_filename_stem(input);
            assert!(
                !stem.contains('/') && !stem.contains(':'),
                "`{input}` gave the stem `{stem}`, which names a path, not a file"
            );
        }
        assert_eq!(module_filename_stem("wasi:http/types"), "wasi-http-types");
        assert_eq!(module_filename_stem("src/app.wado"), "app");
    }

    #[test]
    fn fenced_block_preserves_attribute_lines_as_content() {
        // Lines starting with `#` inside a fenced block are content, not headings.
        let input =
            "Example:\n\n```wado\nstruct S {\n    #[serde(default)]\n    port: i32,\n}\n```";
        let out = render(input);
        assert!(
            out.contains("    #[serde(default)]\n    port: i32,"),
            "expected attribute and field on consecutive lines, got:\n{out}"
        );
        // No spurious blank line between the attribute and the next field.
        assert!(
            !out.contains("#[serde(default)]\n\n"),
            "unexpected blank after attribute:\n{out}"
        );
    }

    #[test]
    fn fenced_block_preserves_indentation_and_alignment() {
        let input = "```wado\nlet a = 1;          // one\nlet bb = 22;        // two\n```";
        let out = render(input);
        assert!(
            out.contains("let a = 1;          // one\n"),
            "alignment whitespace should survive inside fenced block:\n{out}"
        );
        assert!(
            out.contains("let bb = 22;        // two\n"),
            "alignment whitespace should survive inside fenced block:\n{out}"
        );
    }

    #[test]
    fn tilde_fenced_block_is_recognized() {
        let input = "~~~wado\n#[serde(default)]\nport: i32 = 0,\n~~~";
        let out = render(input);
        assert!(
            out.contains("~~~wado\n#[serde(default)]\nport: i32 = 0,\n~~~\n"),
            "tilde fence should pass through content verbatim:\n{out}"
        );
    }

    #[test]
    fn longer_closing_fence_is_accepted() {
        // Opening of length 4 must be matched by 4+ closing chars.
        let input = "````wado\nlet x = 1;\n`````";
        let out = render(input);
        assert!(out.contains("````wado\nlet x = 1;\n`````\n"), "got:\n{out}");
    }

    #[test]
    fn shorter_closing_fence_does_not_close() {
        // Opening of length 4, run of 3 backticks inside is not a close.
        let input = "````wado\n```\nstill code\n````";
        let out = render(input);
        assert!(
            out.contains("```\nstill code\n````\n"),
            "inner ``` should remain content:\n{out}"
        );
    }

    #[test]
    fn outside_fence_heading_still_inserts_blank() {
        // Sanity check: existing heading behaviour outside fenced blocks is unchanged.
        let input = "# Title\nfollow-up";
        let out = render(input);
        assert_eq!(out, "# Title\n\nfollow-up\n");
    }

    #[test]
    fn fence_close_with_info_string_is_not_a_close() {
        // Closing fences disallow info strings per CommonMark — `````wado` is content.
        let input = "```wado\n```rust\nstill code\n```";
        let out = render(input);
        assert!(out.contains("```rust\nstill code\n```\n"), "got:\n{out}");
    }

    #[test]
    fn fence_blank_line_is_preserved() {
        let input = "```wado\nlet a = 1;\n\nlet b = 2;\n```";
        let out = render(input);
        assert!(
            out.contains("let a = 1;\n\nlet b = 2;\n"),
            "blank lines inside fenced block must be preserved:\n{out}"
        );
    }
}
