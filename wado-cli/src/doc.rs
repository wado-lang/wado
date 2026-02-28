use std::fmt::Write as FmtWrite;
use std::fs;
use std::path::Path;
use std::process;

use lexopt::Arg::Value;
use wado_compiler::doc::{
    DocEnum, DocFlags, DocFunction, DocModule, DocStruct, DocTrait, DocVariant, extract_doc,
    extract_stdlib_doc,
};

use crate::args::{self, CliExit};

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
}

#[derive(Clone, Copy)]
enum Opt {
    Format,
    Title,
    Help,
}

impl Opt {
    const ALL: &[Self] = &[Self::Format, Self::Title, Self::Help];

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
    writeln!(buf, "Outputs to stdout.").unwrap();
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

pub fn parse_args(mut parser: lexopt::Parser) -> Result<DocOptions, CliExit> {
    let usage = format_usage();
    let mut inputs: Vec<String> = Vec::new();
    let mut format = OutputFormat::Markdown;
    let mut title: Option<String> = None;

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
                Opt::Help => return Err(CliExit::help(usage)),
            }
        } else if let Value(val) = arg {
            inputs.push(val.to_string_lossy().into_owned());
        } else {
            return Err(args::unexpected_arg(arg, &usage));
        }
    }

    let inputs = args::require_inputs(inputs, &usage)?;

    if inputs.len() > 1 && title.is_none() {
        return Err(CliExit::error(
            "--title is required when generating docs for multiple modules".to_string(),
        ));
    }

    Ok(DocOptions {
        inputs,
        format,
        title,
    })
}

fn is_stdlib_module(input: &str) -> bool {
    input.starts_with("core:") || input.starts_with("wasi:")
}

pub fn run(opts: DocOptions) {
    let mut doc_modules: Vec<DocModule> = Vec::new();

    for input in &opts.inputs {
        if is_stdlib_module(input) {
            // Resolve stdlib module by name (e.g., "core:cli", "wasi:http")
            if let Some(doc) = extract_stdlib_doc(input) {
                doc_modules.push(doc);
            } else {
                eprintln!("Unknown stdlib module: {input}");
                process::exit(1);
            }
            continue;
        }

        let path = Path::new(input);

        let source = match fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("Error reading '{}': {e}", path.display());
                process::exit(1);
            }
        };

        let parsed = match wado_compiler::parse(&source) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("Error parsing '{}': {e:?}", path.display());
                process::exit(1);
            }
        };

        let module_name = path
            .file_stem()
            .map_or("unknown", |s| s.to_str().unwrap_or("unknown"));

        let doc = extract_doc(&parsed.ast, &parsed.comments, module_name);
        doc_modules.push(doc);
    }

    let multi = opts.title.is_some();
    let h_offset: usize = if multi { 1 } else { 0 };

    let format_name = match opts.format {
        OutputFormat::Markdown => "markdown",
        OutputFormat::Simple => "simple",
        OutputFormat::Json => "json",
    };

    match opts.format {
        OutputFormat::Json => {
            if doc_modules.len() == 1 {
                println!("{}", serde_json::to_string_pretty(&doc_modules[0]).unwrap());
            } else {
                println!("{}", serde_json::to_string_pretty(&doc_modules).unwrap());
            }
        }
        OutputFormat::Markdown | OutputFormat::Simple => {
            let mut out = String::new();
            render_auto_generated_comment(&mut out, format_name, &opts);
            if let Some(ref title) = opts.title {
                writeln!(out, "# {title}\n").unwrap();
            }
            for doc in &doc_modules {
                match opts.format {
                    OutputFormat::Markdown => out.push_str(&render_markdown(doc, h_offset)),
                    OutputFormat::Simple => out.push_str(&render_simple(doc, h_offset)),
                    OutputFormat::Json => unreachable!(),
                }
            }
            print!("{out}");
        }
    }
}

fn render_auto_generated_comment(out: &mut String, format_name: &str, opts: &DocOptions) {
    write!(out, "<!-- Auto-generated by: wado doc -f {format_name}").unwrap();
    if let Some(ref title) = opts.title {
        write!(out, " --title \"{title}\"").unwrap();
    }
    for input in &opts.inputs {
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
        out.push_str(module_doc);
        out.push('\n');
    }

    if !doc.traits.is_empty() {
        writeln!(out, "\n{h2} Traits").unwrap();
        for t in &doc.traits {
            render_md_trait(&mut out, t, h3, h4);
        }
    }

    if !doc.structs.is_empty() {
        writeln!(out, "\n{h2} Structs").unwrap();
        for s in &doc.structs {
            render_md_struct(&mut out, s, h3, h4);
        }
    }

    if !doc.types.is_empty() {
        writeln!(out, "\n{h2} Types").unwrap();
        for t in &doc.types {
            writeln!(out, "\n{h3} `{}`", t.signature).unwrap();
            if let Some(ref d) = t.doc {
                writeln!(out, "\n{d}").unwrap();
            }
        }
    }

    if !doc.globals.is_empty() {
        writeln!(out, "\n{h2} Globals").unwrap();
        for g in &doc.globals {
            writeln!(out, "\n{h3} `{}`", g.signature).unwrap();
            if let Some(ref d) = g.doc {
                writeln!(out, "\n{d}").unwrap();
            }
        }
    }

    if !doc.enums.is_empty() {
        writeln!(out, "\n{h2} Enums").unwrap();
        for e in &doc.enums {
            render_md_enum(&mut out, e, h3);
        }
    }

    if !doc.variants.is_empty() {
        writeln!(out, "\n{h2} Variants").unwrap();
        for v in &doc.variants {
            render_md_variant(&mut out, v, h3);
        }
    }

    if !doc.flags.is_empty() {
        writeln!(out, "\n{h2} Flags").unwrap();
        for f in &doc.flags {
            render_md_flags(&mut out, f, h3, h4);
        }
    }

    if !doc.functions.is_empty() {
        writeln!(out, "\n{h2} Functions").unwrap();
        for f in &doc.functions {
            writeln!(out, "\n{h3} `{}`", f.signature).unwrap();
            if let Some(ref d) = f.doc {
                writeln!(out, "\n{d}").unwrap();
            }
        }
    }

    out
}

fn render_md_trait(out: &mut String, t: &DocTrait, h3: &str, h4: &str) {
    writeln!(out, "\n{h3} `{}`", t.signature).unwrap();
    if let Some(ref d) = t.doc {
        writeln!(out, "\n{d}").unwrap();
    }
    if !t.methods.is_empty() {
        writeln!(out, "\n{h4} Methods\n").unwrap();
        for m in &t.methods {
            render_md_method_item(out, m);
        }
    }
}

fn render_md_struct(out: &mut String, s: &DocStruct, h3: &str, h4: &str) {
    writeln!(out, "\n{h3} `{}`", s.signature).unwrap();
    if let Some(ref d) = s.doc {
        writeln!(out, "\n{d}").unwrap();
    }
    if !s.fields.is_empty() {
        writeln!(out, "\n{h4} Fields\n").unwrap();
        for f in &s.fields {
            if let Some(ref d) = f.doc {
                let brief = d.lines().next().unwrap_or("");
                writeln!(out, "- `{}: {}` — {brief}", f.name, f.ty).unwrap();
            } else {
                writeln!(out, "- `{}: {}`", f.name, f.ty).unwrap();
            }
        }
    }
    if !s.methods.is_empty() {
        writeln!(out, "\n{h4} Methods\n").unwrap();
        for m in &s.methods {
            render_md_method_item(out, m);
        }
    }
    for ti in &s.trait_impls {
        writeln!(out, "\n{h4} `{}`\n", ti.signature).unwrap();
        for m in &ti.methods {
            render_md_method_item(out, m);
        }
    }
}

fn render_md_method_item(out: &mut String, m: &DocFunction) {
    if let Some(ref d) = m.doc {
        let brief = d.lines().next().unwrap_or("");
        writeln!(out, "- `{}` — {brief}", m.signature).unwrap();
    } else {
        writeln!(out, "- `{}`", m.signature).unwrap();
    }
}

fn render_md_enum(out: &mut String, e: &DocEnum, h3: &str) {
    writeln!(out, "\n{h3} `{}`", e.signature).unwrap();
    if let Some(ref d) = e.doc {
        writeln!(out, "\n{d}").unwrap();
    }
}

fn render_md_variant(out: &mut String, v: &DocVariant, h3: &str) {
    writeln!(out, "\n{h3} `{}`", v.signature).unwrap();
    if let Some(ref d) = v.doc {
        writeln!(out, "\n{d}").unwrap();
    }
    for case in &v.cases {
        let case_repr = if let Some(ref p) = case.payload {
            format!("{}({p})", case.name)
        } else {
            case.name.clone()
        };
        if let Some(ref d) = case.doc {
            let brief = d.lines().next().unwrap_or("");
            writeln!(out, "- `{case_repr}` — {brief}").unwrap();
        } else {
            writeln!(out, "- `{case_repr}`").unwrap();
        }
    }
}

fn render_md_flags(out: &mut String, f: &DocFlags, h3: &str, h4: &str) {
    writeln!(out, "\n{h3} `{}`", f.signature).unwrap();
    if let Some(ref d) = f.doc {
        writeln!(out, "\n{d}").unwrap();
    }
    writeln!(out, "\n{h4} Members\n").unwrap();
    for member in &f.members {
        writeln!(out, "- `{member}`").unwrap();
    }
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

    if !doc.traits.is_empty() {
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

    if !doc.structs.is_empty() {
        writeln!(out, "\n{h2} Structs").unwrap();
        for s in &doc.structs {
            out.push_str("\n```wado\n");
            render_simple_struct(&mut out, s);
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

    if !doc.types.is_empty() {
        writeln!(out, "\n{h2} Types\n\n```wado").unwrap();
        for t in &doc.types {
            writeln!(out, "{};", t.signature).unwrap();
        }
        out.push_str("```\n");
    }

    if !doc.globals.is_empty() {
        writeln!(out, "\n{h2} Globals\n\n```wado").unwrap();
        for g in &doc.globals {
            writeln!(out, "{};", g.signature).unwrap();
        }
        out.push_str("```\n");
    }

    if !doc.enums.is_empty() {
        writeln!(out, "\n{h2} Enums").unwrap();
        for e in &doc.enums {
            writeln!(out, "\n```wado\n{}\n```", e.signature).unwrap();
        }
    }

    if !doc.variants.is_empty() {
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

    if !doc.flags.is_empty() {
        writeln!(out, "\n{h2} Flags").unwrap();
        for f in &doc.flags {
            writeln!(out, "\n```wado\n{} {{", f.signature).unwrap();
            for member in &f.members {
                writeln!(out, "    {member},").unwrap();
            }
            out.push_str("}\n```\n");
        }
    }

    if !doc.functions.is_empty() {
        writeln!(out, "\n{h2} Functions\n\n```wado").unwrap();
        for f in &doc.functions {
            writeln!(out, "{};", f.signature).unwrap();
        }
        out.push_str("```\n");
    }

    out
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
