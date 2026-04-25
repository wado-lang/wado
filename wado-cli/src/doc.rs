use std::fmt::Write as FmtWrite;
use std::fs;
use std::path::Path;
use std::process;

use lexopt::Arg::Value;
use wado_compiler::doc::{
    DocEffect, DocEnum, DocFlags, DocModule, DocPrimitiveType, DocResource, DocStruct, DocTrait,
    DocVariant, extract_doc, extract_stdlib_doc,
};

use crate::args::{self, CliExit};

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
}

#[derive(Clone, Copy)]
enum Opt {
    Format,
    Title,
    Output,
    Help,
}

impl Opt {
    const ALL: &[Self] = &[Self::Format, Self::Title, Self::Output, Self::Help];

    const fn spec(self) -> args::OptSpec {
        match self {
            Self::Format => args::OptSpec {
                long: Some("format"),
                short: Some('f'),
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

pub fn print_usage() {
    eprint!("{}", format_usage());
}

/// Parse command-line arguments for the `doc` subcommand.
///
/// # Errors
///
/// Returns an error if the arguments are invalid or required arguments are missing.
pub fn parse_args(mut parser: lexopt::Parser) -> Result<DocOptions, CliExit> {
    let usage = format_usage();
    let mut inputs: Vec<String> = Vec::new();
    let mut format = OutputFormat::Markdown;
    let mut title: Option<String> = None;
    let mut output: Option<String> = None;

    while let Some(arg) = args::next_arg(&mut parser)? {
        if let Some(opt) = args::match_opt(&arg, Opt::ALL, |o| o.spec()) {
            match opt {
                Opt::Format => {
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
                Opt::Help => return Err(CliExit::help(usage)),
            }
        } else if let Value(val) = arg {
            inputs.push(val.to_string_lossy().into_owned());
        } else {
            return Err(args::unexpected_arg(arg, &usage));
        }
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
    })
}

fn is_stdlib_module(input: &str) -> bool {
    input.starts_with("core:") || input.starts_with("wasi:")
}

pub fn run(opts: DocOptions) {
    let docs: Vec<(String, DocModule)> = opts
        .inputs
        .iter()
        .map(|input| (input.clone(), load_doc(input)))
        .collect();

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
            write_to_file(&path, &content);
        }
        return;
    }

    let combined = render_combined(&docs, opts.title.as_deref(), format_name, &opts);
    match opts.output.as_deref() {
        Some(path) => write_to_file(path, &combined),
        None => print!("{combined}"),
    }
}

fn load_doc(input: &str) -> DocModule {
    if is_stdlib_module(input) {
        return extract_stdlib_doc(input).unwrap_or_else(|| {
            eprintln!("Unknown stdlib module: {input}");
            process::exit(1);
        });
    }

    let path = Path::new(input);
    let source = fs::read_to_string(path).unwrap_or_else(|e| {
        eprintln!("Error reading '{}': {e}", path.display());
        process::exit(1);
    });

    let parsed = wado_compiler::parse(&source).unwrap_or_else(|e| {
        eprintln!("Error parsing '{}': {e:?}", path.display());
        process::exit(1);
    });

    let module_name = path
        .file_stem()
        .map_or("unknown", |s| s.to_str().unwrap_or("unknown"));

    extract_doc(&parsed.ast, &parsed.comments, module_name)
}

/// Filesystem-safe stem for a module input: stdlib `core:cli` -> `core-cli`,
/// file paths -> file stem.
fn module_filename_stem(input: &str) -> String {
    if is_stdlib_module(input) {
        return input.replace(':', "-");
    }
    Path::new(input)
        .file_stem()
        .and_then(|s| s.to_str())
        .map_or_else(|| input.to_string(), str::to_string)
}

fn write_to_file(path: &str, content: &str) {
    if let Some(parent) = Path::new(path).parent()
        && !parent.as_os_str().is_empty()
        && let Err(e) = fs::create_dir_all(parent)
    {
        eprintln!("Error creating '{}': {e}", parent.display());
        process::exit(1);
    }
    if let Err(e) = fs::write(path, content) {
        eprintln!("Error writing '{path}': {e}");
        process::exit(1);
    }
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
            out
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
    out
}

fn render_auto_generated_header(
    out: &mut String,
    format_name: &str,
    title: Option<&str>,
    inputs: &[String],
) {
    write!(out, "<!-- Auto-generated by: wado doc -f {format_name}").unwrap();
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

fn render_md_types_section(out: &mut String, doc: &DocModule, h2: &str, h3: &str) {
    if doc.types.is_empty() {
        return;
    }
    writeln!(out, "\n{h2} Types").unwrap();
    for t in &doc.types {
        writeln!(out, "\n{h3} `{}`", t.signature).unwrap();
        if let Some(ref d) = t.doc {
            out.push('\n');
            render_md_doc(out, d);
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
    for c in &p.constants {
        render_md_entity(out, &c.signature, c.doc.as_deref(), h4);
    }
    for m in &p.methods {
        render_md_entity(out, &m.signature, m.doc.as_deref(), h4);
    }
    for ti in &p.trait_impls {
        writeln!(out, "\n{h4} `{}`", ti.signature).unwrap();
        let h5 = h(heading_level(h4) + 1);
        for m in &ti.methods {
            render_md_entity(out, &m.signature, m.doc.as_deref(), h5);
        }
    }
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
        writeln!(out, "\n{h3} `{}`", f.signature).unwrap();
        if let Some(ref d) = f.doc {
            out.push('\n');
            render_md_doc(out, d);
        }
    }
}

fn render_md_trait(out: &mut String, t: &DocTrait, h3: &str, h4: &str) {
    writeln!(out, "\n{h3} `{}`", t.signature).unwrap();
    if let Some(ref d) = t.doc {
        out.push('\n');
        render_md_doc(out, d);
    }
    for m in &t.methods {
        render_md_entity(out, &m.signature, m.doc.as_deref(), h4);
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
    for m in &s.methods {
        render_md_entity(out, &m.signature, m.doc.as_deref(), h4);
    }
    for ti in &s.trait_impls {
        writeln!(out, "\n{h4} `{}`", ti.signature).unwrap();
        let h5 = h(heading_level(h4) + 1);
        for m in &ti.methods {
            render_md_entity(out, &m.signature, m.doc.as_deref(), h5);
        }
    }
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
}

fn render_md_variant(out: &mut String, v: &DocVariant, h3: &str, h4: &str) {
    writeln!(out, "\n{h3} `{}`", v.signature).unwrap();
    if let Some(ref d) = v.doc {
        out.push('\n');
        render_md_doc(out, d);
    }
    for case in &v.cases {
        let case_repr = if let Some(ref p) = case.payload {
            format!("{}({p})", case.name)
        } else {
            case.name.clone()
        };
        render_md_entity(out, &case_repr, case.doc.as_deref(), h4);
    }
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
}

fn render_md_effect(out: &mut String, e: &DocEffect, h3: &str, h4: &str) {
    writeln!(out, "\n{h3} `{}`", e.signature).unwrap();
    if let Some(ref d) = e.doc {
        out.push('\n');
        render_md_doc(out, d);
    }
    for m in &e.methods {
        render_md_entity(out, &m.signature, m.doc.as_deref(), h4);
    }
}

fn render_md_resource(out: &mut String, r: &DocResource, h3: &str, h4: &str) {
    writeln!(out, "\n{h3} `{}`", r.signature).unwrap();
    if let Some(ref d) = r.doc {
        out.push('\n');
        render_md_doc(out, d);
    }
    for m in &r.methods {
        render_md_entity(out, &m.signature, m.doc.as_deref(), h4);
    }
}

fn render_md_entity(out: &mut String, sig: &str, doc: Option<&str>, h: &str) {
    writeln!(out, "\n{h} `{sig}`").unwrap();
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

    for line in doc.lines() {
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
    let collapsed = collapse_md_spaces(stripped);
    replace_md_emphasis(&collapsed)
}

/// Collapse multiple consecutive spaces to single space, preserving code spans.
fn collapse_md_spaces(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut in_code = false;
    let mut prev_space = false;

    for c in s.chars() {
        if c == '`' {
            in_code = !in_code;
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

/// Replace `*text*` emphasis with `_text_` (dprint preference), preserving
/// `**bold**` markers and code spans.
fn replace_md_emphasis(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    let mut in_code = false;

    while i < chars.len() {
        if chars[i] == '`' {
            in_code = !in_code;
            result.push('`');
            i += 1;
        } else if !in_code && chars[i] == '*' {
            if i + 1 < chars.len() && chars[i + 1] == '*' {
                result.push('*');
                result.push('*');
                i += 2;
            } else {
                result.push('_');
                i += 1;
            }
        } else {
            result.push(chars[i]);
            i += 1;
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
    }
    out.push_str("```\n");
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
        out.push_str("}\n```\n");
    }
}

fn render_simple_enums_section(out: &mut String, doc: &DocModule, h2: &str) {
    if doc.enums.is_empty() {
        return;
    }
    writeln!(out, "\n{h2} Enums").unwrap();
    for e in &doc.enums {
        let name = sig_name_part(&e.signature);
        writeln!(out, "\n```wado\n{name} {{").unwrap();
        for case in &e.cases {
            writeln!(out, "    {},", case.name).unwrap();
        }
        out.push_str("}\n```\n");
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
            writeln!(out, "    {};", m.signature).unwrap();
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
        if r.methods.is_empty() {
            writeln!(out, "\n```wado\n{};\n```", r.signature).unwrap();
        } else {
            writeln!(out, "\n```wado\n{} {{", r.signature).unwrap();
            for m in &r.methods {
                writeln!(out, "    {};", m.signature).unwrap();
            }
            out.push_str("}\n```\n");
        }
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
            writeln!(out, "    type {assoc};").unwrap();
        }
        for m in &t.methods {
            writeln!(out, "    {};", m.signature).unwrap();
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
        if !s.methods.is_empty() {
            let type_name = s
                .signature
                .strip_prefix("pub ")
                .unwrap_or(&s.signature)
                .strip_prefix("struct ")
                .and_then(|rest| rest.split([' ', '<', '{']).next())
                .unwrap_or("?");
            writeln!(out, "\nimpl {type_name} {{").unwrap();
            for m in &s.methods {
                writeln!(out, "    {};", m.signature).unwrap();
            }
            out.push_str("}\n");
        }
        for ti in &s.trait_impls {
            writeln!(out, "\n{} {{", ti.signature).unwrap();
            for m in &ti.methods {
                writeln!(out, "    {};", m.signature).unwrap();
            }
            out.push_str("}\n");
        }
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
        writeln!(out, "impl {} {{", p.name).unwrap();
        for c in &p.constants {
            writeln!(out, "    {};", c.signature).unwrap();
        }
        for m in &p.methods {
            writeln!(out, "    {};", m.signature).unwrap();
        }
        out.push_str("}\n");
        for ti in &p.trait_impls {
            writeln!(out, "\n{} {{", ti.signature).unwrap();
            for m in &ti.methods {
                writeln!(out, "    {};", m.signature).unwrap();
            }
            out.push_str("}\n");
        }
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
            if let Some(ref p) = case.payload {
                writeln!(out, "    {}({p}),", case.name).unwrap();
            } else {
                writeln!(out, "    {},", case.name).unwrap();
            }
        }
        out.push_str("}\n```\n");
    }
}

fn render_simple_functions_section(out: &mut String, doc: &DocModule, h2: &str) {
    if doc.functions.is_empty() {
        return;
    }
    writeln!(out, "\n{h2} Functions\n\n```wado").unwrap();
    for f in &doc.functions {
        writeln!(out, "{};", f.signature).unwrap();
    }
    out.push_str("```\n");
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
