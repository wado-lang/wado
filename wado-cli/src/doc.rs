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
}

#[derive(Clone, Copy)]
enum Opt {
    Format,
    Help,
}

impl Opt {
    const ALL: &[Self] = &[Self::Format, Self::Help];

    const fn spec(self) -> args::OptSpec {
        match self {
            Self::Format => args::OptSpec {
                long: Some("format"),
                short: Some('f'),
                value: Some("<fmt>"),
                desc: "Output format: markdown (default), simple, json",
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
                Opt::Help => return Err(CliExit::help(usage)),
            }
        } else if let Value(val) = arg {
            inputs.push(val.to_string_lossy().into_owned());
        } else {
            return Err(args::unexpected_arg(arg, &usage));
        }
    }

    let inputs = args::require_inputs(inputs, &usage)?;

    Ok(DocOptions { inputs, format })
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
                doc_modules.push(doc)
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

    match opts.format {
        OutputFormat::Json => {
            if doc_modules.len() == 1 {
                println!("{}", serde_json::to_string_pretty(&doc_modules[0]).unwrap());
            } else {
                println!("{}", serde_json::to_string_pretty(&doc_modules).unwrap());
            }
        }
        OutputFormat::Markdown => {
            let mut first = true;
            for doc in &doc_modules {
                if !first {
                    println!();
                }
                print!("{}", render_markdown(doc));
                first = false;
            }
        }
        OutputFormat::Simple => {
            let mut first = true;
            for doc in &doc_modules {
                if !first {
                    println!();
                }
                print!("{}", render_simple(doc));
                first = false;
            }
        }
    }
}

fn render_markdown(doc: &DocModule) -> String {
    let mut out = format!("# {}\n", doc.name);

    if let Some(ref module_doc) = doc.doc {
        out.push('\n');
        out.push_str(module_doc);
        out.push('\n');
    }

    if !doc.traits.is_empty() {
        out.push_str("\n## Traits\n");
        for t in &doc.traits {
            render_md_trait(&mut out, t);
        }
    }

    if !doc.structs.is_empty() {
        out.push_str("\n## Structs\n");
        for s in &doc.structs {
            render_md_struct(&mut out, s);
        }
    }

    if !doc.types.is_empty() {
        out.push_str("\n## Types\n");
        for t in &doc.types {
            writeln!(out, "\n### `{}`", t.signature).unwrap();
            if let Some(ref d) = t.doc {
                writeln!(out, "\n{d}").unwrap();
            }
        }
    }

    if !doc.globals.is_empty() {
        out.push_str("\n## Globals\n");
        for g in &doc.globals {
            writeln!(out, "\n### `{}`", g.signature).unwrap();
            if let Some(ref d) = g.doc {
                writeln!(out, "\n{d}").unwrap();
            }
        }
    }

    if !doc.enums.is_empty() {
        out.push_str("\n## Enums\n");
        for e in &doc.enums {
            render_md_enum(&mut out, e);
        }
    }

    if !doc.variants.is_empty() {
        out.push_str("\n## Variants\n");
        for v in &doc.variants {
            render_md_variant(&mut out, v);
        }
    }

    if !doc.flags.is_empty() {
        out.push_str("\n## Flags\n");
        for f in &doc.flags {
            render_md_flags(&mut out, f);
        }
    }

    if !doc.functions.is_empty() {
        out.push_str("\n## Functions\n");
        for f in &doc.functions {
            writeln!(out, "\n### `{}`", f.signature).unwrap();
            if let Some(ref d) = f.doc {
                writeln!(out, "\n{d}").unwrap();
            }
        }
    }

    out
}

fn render_md_trait(out: &mut String, t: &DocTrait) {
    writeln!(out, "\n### `{}`", t.signature).unwrap();
    if let Some(ref d) = t.doc {
        writeln!(out, "\n{d}").unwrap();
    }
    if !t.methods.is_empty() {
        writeln!(out, "\n#### Methods\n").unwrap();
        for m in &t.methods {
            render_md_method_item(out, m);
        }
    }
}

fn render_md_struct(out: &mut String, s: &DocStruct) {
    writeln!(out, "\n### `{}`", s.signature).unwrap();
    if let Some(ref d) = s.doc {
        writeln!(out, "\n{d}").unwrap();
    }
    if !s.fields.is_empty() {
        writeln!(out, "\n#### Fields\n").unwrap();
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
        writeln!(out, "\n#### Methods\n").unwrap();
        for m in &s.methods {
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

fn render_md_enum(out: &mut String, e: &DocEnum) {
    writeln!(out, "\n### `{}`", e.signature).unwrap();
    if let Some(ref d) = e.doc {
        writeln!(out, "\n{d}").unwrap();
    }
}

fn render_md_variant(out: &mut String, v: &DocVariant) {
    writeln!(out, "\n### `{}`", v.signature).unwrap();
    if let Some(ref d) = v.doc {
        writeln!(out, "\n{d}").unwrap();
    }
    writeln!(out, "\n#### Cases\n").unwrap();
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

fn render_md_flags(out: &mut String, f: &DocFlags) {
    writeln!(out, "\n### `{}`", f.signature).unwrap();
    if let Some(ref d) = f.doc {
        writeln!(out, "\n{d}").unwrap();
    }
    writeln!(out, "\n#### Members\n").unwrap();
    for member in &f.members {
        writeln!(out, "- `{member}`").unwrap();
    }
}

fn render_simple(doc: &DocModule) -> String {
    let mut out = format!("# {}\n", doc.name);

    if let Some(ref module_doc) = doc.doc {
        out.push('\n');
        out.push_str(module_doc);
        out.push('\n');
    }

    if !doc.traits.is_empty() {
        out.push_str("\n## Traits\n\n```wado\n");
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
        out.push_str("\n## Structs\n\n```wado\n");
        for (i, s) in doc.structs.iter().enumerate() {
            if i > 0 {
                out.push('\n');
            }
            render_simple_struct(&mut out, s);
        }
        out.push_str("```\n");

        for s in &doc.structs {
            if s.methods.is_empty() {
                continue;
            }
            // Extract type name from signature (after "struct ")
            let type_name = s
                .signature
                .strip_prefix("pub ")
                .unwrap_or(&s.signature)
                .strip_prefix("struct ")
                .and_then(|rest| rest.split([' ', '<', '{']).next())
                .unwrap_or("?");
            writeln!(out, "\n```wado\nimpl {type_name} {{").unwrap();
            for m in &s.methods {
                writeln!(out, "    {};", m.signature).unwrap();
            }
            out.push_str("}\n```\n");
        }
    }

    if !doc.types.is_empty() {
        out.push_str("\n## Types\n\n```wado\n");
        for t in &doc.types {
            writeln!(out, "{};", t.signature).unwrap();
        }
        out.push_str("```\n");
    }

    if !doc.globals.is_empty() {
        out.push_str("\n## Globals\n\n```wado\n");
        for g in &doc.globals {
            writeln!(out, "{};", g.signature).unwrap();
        }
        out.push_str("```\n");
    }

    if !doc.enums.is_empty() {
        out.push_str("\n## Enums\n\n```wado\n");
        for e in &doc.enums {
            writeln!(out, "{}", e.signature).unwrap();
        }
        out.push_str("```\n");
    }

    if !doc.variants.is_empty() {
        out.push_str("\n## Variants\n\n```wado\n");
        for v in &doc.variants {
            writeln!(out, "{} {{", v.signature).unwrap();
            for case in &v.cases {
                if let Some(ref p) = case.payload {
                    writeln!(out, "    {}({p}),", case.name).unwrap();
                } else {
                    writeln!(out, "    {},", case.name).unwrap();
                }
            }
            out.push_str("}\n");
        }
        out.push_str("```\n");
    }

    if !doc.flags.is_empty() {
        out.push_str("\n## Flags\n\n```wado\n");
        for f in &doc.flags {
            writeln!(out, "{} {{", f.signature).unwrap();
            for member in &f.members {
                writeln!(out, "    {member},").unwrap();
            }
            out.push_str("}\n");
        }
        out.push_str("```\n");
    }

    if !doc.functions.is_empty() {
        out.push_str("\n## Functions\n\n```wado\n");
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
