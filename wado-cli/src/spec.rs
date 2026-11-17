//! `wado spec`: the specification and the cheatsheet, bundled into the binary.
//! See [WEP: The `spec` Command](../../docs/wep-2026-11-17-spec-command.md).

use std::fmt::Write as _;

use lexopt::Arg::Value;

use crate::args::{self, CliExit};
use crate::markdown::{fence_open, matches_fence_close};
use crate::search::{self, Query};

/// Every bundled document, by the name `wado spec` takes, in reading order.
const DOCUMENTS: &[(&str, &str)] = &[
    ("cheatsheet", include_str!("../../docs/cheatsheet.md")),
    ("overview", include_str!("../../docs/spec-overview.md")),
    ("lexical", include_str!("../../docs/spec-lexical.md")),
    ("literals", include_str!("../../docs/spec-literals.md")),
    (
        "expressions",
        include_str!("../../docs/spec-expressions.md"),
    ),
    ("types", include_str!("../../docs/spec-types.md")),
    ("patterns", include_str!("../../docs/spec-patterns.md")),
    (
        "control-flow",
        include_str!("../../docs/spec-control-flow.md"),
    ),
    ("assertions", include_str!("../../docs/spec-assertions.md")),
    ("functions", include_str!("../../docs/spec-functions.md")),
    ("memory", include_str!("../../docs/spec-memory.md")),
    ("traits", include_str!("../../docs/spec-traits.md")),
    (
        "standard-traits",
        include_str!("../../docs/spec-standard-traits.md"),
    ),
    ("affine", include_str!("../../docs/spec-affine.md")),
    ("reflection", include_str!("../../docs/spec-reflection.md")),
    ("effects", include_str!("../../docs/spec-effects.md")),
    ("modules", include_str!("../../docs/spec-modules.md")),
    ("packages", include_str!("../../docs/spec-packages.md")),
    ("kiln", include_str!("../../docs/spec-kiln.md")),
    ("worlds", include_str!("../../docs/spec-worlds.md")),
    ("components", include_str!("../../docs/spec-components.md")),
    (
        "serialization",
        include_str!("../../docs/spec-serialization.md"),
    ),
    ("attributes", include_str!("../../docs/spec-attributes.md")),
    (
        "diagnostics",
        include_str!("../../docs/spec-diagnostics.md"),
    ),
    ("testing", include_str!("../../docs/spec-testing.md")),
    ("pgo", include_str!("../../docs/spec-pgo.md")),
];

/// A heading and the lines it heads, up to the next heading of any level.
struct Section<'a> {
    level: usize,
    title: &'a str,
    slug: String,
    /// Index of the heading's line.
    start: usize,
    /// Index one past the last line of the section and its subsections.
    end: usize,
    /// The lines under the heading, up to the next heading of any level.
    body: String,
}

/// A bundled document split into its sections.
struct Document<'a> {
    name: &'static str,
    text: &'a str,
    lines: Vec<&'a str>,
    sections: Vec<Section<'a>>,
}

impl<'a> Document<'a> {
    fn parse(name: &'static str, text: &'a str) -> Self {
        let lines: Vec<&str> = text.lines().collect();
        let mut headings: Vec<(usize, usize, &str)> = Vec::new();
        let mut fence = None;
        for (i, line) in lines.iter().enumerate() {
            let trimmed = line.trim_start();
            if let Some(open) = fence {
                if matches_fence_close(trimmed, &open) {
                    fence = None;
                }
                continue;
            }
            if let Some(open) = fence_open(trimmed) {
                fence = Some(open);
                continue;
            }
            if let Some((level, title)) = heading(line) {
                headings.push((i, level, title));
            }
        }

        let mut seen: Vec<String> = Vec::new();
        let sections = headings
            .iter()
            .enumerate()
            .map(|(k, &(start, level, title))| {
                let next = headings.get(k + 1).map_or(lines.len(), |h| h.0);
                let end = headings[k + 1..]
                    .iter()
                    .find(|h| h.1 <= level)
                    .map_or(lines.len(), |h| h.0);
                let base = slug(title);
                let repeats = seen.iter().filter(|s| **s == base).count();
                seen.push(base.clone());
                Section {
                    level,
                    title,
                    slug: if repeats == 0 {
                        base
                    } else {
                        format!("{base}-{repeats}")
                    },
                    start,
                    end,
                    body: lines[start + 1..next].join("\n"),
                }
            })
            .collect();
        Self {
            name,
            text,
            lines,
            sections,
        }
    }

    fn title(&self) -> &str {
        let first = &self.sections[0];
        assert_eq!(first.level, 1, "a document opens with its title");
        first.title
    }

    fn section(&self, slug: &str) -> Option<&Section<'a>> {
        self.sections.iter().find(|s| s.slug == slug)
    }

    fn text_of(&self, section: &Section) -> String {
        let mut text = self.lines[section.start..section.end].join("\n");
        text.push('\n');
        text
    }

    /// The argument of `wado spec` that prints `section`.
    fn address(&self, section: &Section) -> String {
        if section.level == 1 {
            self.name.to_string()
        } else {
            format!("{}#{}", self.name, section.slug)
        }
    }
}

/// The level and text of an ATX heading.
fn heading(line: &str) -> Option<(usize, &str)> {
    let level = line.chars().take_while(|c| *c == '#').count();
    let rest = line[level..].strip_prefix(' ')?;
    (1..=6).contains(&level).then(|| (level, rest.trim()))
}

/// The anchor GitHub gives a heading: lowercase, punctuation dropped, spaces
/// turned into `-`.
fn slug(title: &str) -> String {
    title
        .to_lowercase()
        .chars()
        .filter_map(|c| match c {
            ' ' => Some('-'),
            c if c.is_alphanumeric() || c == '-' || c == '_' => Some(c),
            _ => None,
        })
        .collect()
}

fn documents() -> Vec<Document<'static>> {
    DOCUMENTS
        .iter()
        .map(|(name, text)| Document::parse(name, text))
        .collect()
}

pub struct SpecOptions {
    target: Option<String>,
    search: Option<String>,
}

#[derive(Clone, Copy)]
enum Opt {
    Search,
    Help,
}

impl Opt {
    const ALL: &[Self] = &[Self::Search, Self::Help];

    const fn spec(self) -> args::OptSpec {
        match self {
            Self::Search => args::OptSpec {
                long: Some("search"),
                short: Some('s'),
                value: Some("<query>"),
                desc: "List the sections holding every term of the query, \
                       within the document or section given",
            },
            Self::Help => args::HELP_SPEC,
        }
    }
}

fn usage() -> String {
    let mut buf = String::new();
    writeln!(buf, "Usage: wado spec [options] [<name>[#section]]").unwrap();
    writeln!(buf).unwrap();
    writeln!(
        buf,
        "Print the language specification or the cheatsheet bundled into this binary."
    )
    .unwrap();
    writeln!(
        buf,
        "With no name, list the documents. A section is named by its GitHub anchor."
    )
    .unwrap();
    writeln!(buf).unwrap();
    writeln!(buf, "Options:").unwrap();
    write!(buf, "{}", args::format_opts_help(Opt::ALL, |o| o.spec())).unwrap();
    buf
}

pub fn parse_args(mut parser: lexopt::Parser) -> Result<SpecOptions, CliExit> {
    let usage = usage();
    let mut target = None;
    let mut search = None;
    while let Some(arg) = args::next_arg(&mut parser)? {
        if let Some(opt) = args::match_opt(&arg, Opt::ALL, |o| o.spec()) {
            match opt {
                Opt::Search => search = Some(args::require_string(&mut parser)?),
                Opt::Help => return Err(CliExit::help(usage)),
            }
        } else if let Value(val) = &arg
            && target.is_none()
        {
            target = Some(val.to_string_lossy().into_owned());
        } else {
            return Err(args::unexpected_arg(arg, &usage));
        }
    }
    Ok(SpecOptions { target, search })
}

pub fn run(opts: &SpecOptions) -> Result<(), CliExit> {
    let docs = documents();
    let target = opts
        .target
        .as_deref()
        .map(|target| resolve(&docs, target))
        .transpose()?;
    match (&opts.search, target) {
        (Some(query), Some(target)) => print!("{}", search(target.sections(), query)?),
        (Some(query), None) => {
            let sections = docs.iter().flat_map(|doc| Target::whole(doc).sections());
            print!("{}", search(sections, query)?);
        }
        (None, Some(target)) => print!("{}", target.text()),
        (None, None) => print!("{}", index(&docs)),
    }
    Ok(())
}

/// What an argument of `wado spec` names: a document, or one section of it.
#[derive(Clone, Copy)]
struct Target<'d> {
    doc: &'d Document<'d>,
    section: Option<&'d Section<'d>>,
}

impl<'d> Target<'d> {
    fn whole(doc: &'d Document<'d>) -> Self {
        Self { doc, section: None }
    }

    fn text(&self) -> String {
        match self.section {
            Some(section) => self.doc.text_of(section),
            None => self.doc.text.to_string(),
        }
    }

    /// The sections the target spans, each with its document.
    fn sections(self) -> impl Iterator<Item = (&'d Document<'d>, &'d Section<'d>)> {
        let span = self
            .section
            .map_or(0..self.doc.lines.len(), |s| s.start..s.end);
        self.doc
            .sections
            .iter()
            .filter(move |s| span.contains(&s.start))
            .map(move |s| (self.doc, s))
    }
}

fn resolve<'d>(docs: &'d [Document<'d>], target: &str) -> Result<Target<'d>, CliExit> {
    let (name, anchor) = match target.split_once('#') {
        Some((name, anchor)) => (name, Some(anchor)),
        None => (target, None),
    };
    let doc = docs.iter().find(|d| d.name == name).ok_or_else(|| {
        CliExit::error(format!(
            "no document named `{name}`; `wado spec` lists them"
        ))
    })?;
    let Some(anchor) = anchor else {
        return Ok(Target::whole(doc));
    };
    let section = doc.section(anchor).ok_or_else(|| {
        CliExit::error(format!(
            "no section `{anchor}` in `{name}`; `wado spec --search` finds one"
        ))
    })?;
    Ok(Target {
        doc,
        section: Some(section),
    })
}

fn index(docs: &[Document]) -> String {
    let width = docs
        .iter()
        .map(|d| d.name.len())
        .max()
        .expect("documents are bundled");
    let mut out = String::new();
    for doc in docs {
        writeln!(out, "{:width$}  {}", doc.name, doc.title()).unwrap();
    }
    writeln!(
        out,
        "\n`wado spec <name>[#section]` prints one; \
         `wado spec --search <query>` searches them all."
    )
    .unwrap();
    out
}

fn search<'d>(
    sections: impl Iterator<Item = (&'d Document<'d>, &'d Section<'d>)>,
    query: &str,
) -> Result<String, CliExit> {
    let query = Query::parse(query).ok_or_else(|| CliExit::error("the query is empty"))?;
    let hits = sections
        .filter_map(|(doc, section)| {
            let score = query.score(&[(section.title, 10), (&section.body, 1)])?;
            Some((score, (doc, section)))
        })
        .collect();
    let hits = search::rank(hits);
    if hits.is_empty() {
        return Err(CliExit::error(
            "nothing in the specification matches; \
             `wado doc --search` searches the standard library's API",
        ));
    }
    let mut out = String::new();
    for (doc, section) in hits.iter().take(search::SHOWN_HITS) {
        let place = if section.level == 1 {
            section.title.to_string()
        } else {
            format!("{} › {}", doc.title(), section.title)
        };
        writeln!(out, "{}  {place}", doc.address(section)).unwrap();
        if let Some(line) = query.snippet(&section.body) {
            writeln!(out, "    {line}").unwrap();
        }
    }
    if let Some(rest) = search::rest_line(hits.len()) {
        writeln!(out, "{rest}").unwrap();
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The file `name` is bundled from, relative to `docs/`.
    fn file_name(name: &str) -> String {
        if name == "cheatsheet" {
            "cheatsheet.md".to_string()
        } else {
            format!("spec-{name}.md")
        }
    }

    /// The targets of the links in `text` to a file beside it.
    fn relative_links(text: &str) -> impl Iterator<Item = &str> {
        text.match_indices("](./").map(|(i, link)| {
            let rest = &text[i + link.len()..];
            &rest[..rest.find(')').unwrap()]
        })
    }

    /// The overview's chapter list names every spec file in reading order
    /// (`mise run update-docs-index` holds that), so the bundle follows it.
    #[test]
    fn every_chapter_is_bundled_in_reading_order() {
        let docs = documents();
        let overview = docs.iter().find(|d| d.name == "overview").unwrap();
        let chapters = &overview.section("chapters").unwrap().body;
        let mut expected = vec!["cheatsheet.md", "spec-overview.md"];
        expected.extend(relative_links(chapters));
        let bundled: Vec<String> = DOCUMENTS.iter().map(|(n, _)| file_name(n)).collect();
        assert_eq!(bundled, expected);
    }

    /// A link between bundled documents names a section `wado spec` finds, so
    /// the anchors it computes are GitHub's.
    #[test]
    fn every_link_between_bundled_documents_resolves() {
        let docs = documents();
        let mut broken = Vec::new();
        for (name, text) in DOCUMENTS {
            for target in relative_links(text) {
                let Some((file, anchor)) = target.split_once('#') else {
                    continue;
                };
                let Some(doc) = docs.iter().find(|d| file_name(d.name) == file) else {
                    continue;
                };
                if doc.section(anchor).is_none() {
                    broken.push(format!("{name}: {target}"));
                }
            }
        }
        assert!(
            broken.is_empty(),
            "unresolved links:\n{}",
            broken.join("\n")
        );
    }

    #[test]
    fn slug_follows_github() {
        assert_eq!(slug("Let Else"), "let-else");
        assert_eq!(
            slug("`#[param]` / `#[param(from_env = \"...\")]`"),
            "param--paramfrom_env--"
        );
        assert_eq!(
            slug("Tuple for-of (compile-time expansion)"),
            "tuple-for-of-compile-time-expansion"
        );
    }

    const SAMPLE: &str = "# Title\n\nIntro.\n\n## Usage\n\nUse it.\n\n```wado\n# not a heading\n```\n\n### Detail\n\nMore.\n\n## Usage\n\nAgain.\n";

    #[test]
    fn a_heading_in_a_fence_is_content() {
        let doc = Document::parse("sample", SAMPLE);
        let slugs: Vec<&str> = doc.sections.iter().map(|s| s.slug.as_str()).collect();
        assert_eq!(slugs, ["title", "usage", "detail", "usage-1"]);
    }

    /// A backtick fence's info string holds no backtick, so a line opening
    /// with an inline span opens no fence.
    #[test]
    fn an_inline_span_opens_no_fence() {
        let doc = Document::parse("sample", "# Title\n\n```a``` is code.\n\n## After\n");
        assert!(doc.section("after").is_some());
    }

    #[test]
    fn a_section_holds_its_subsections() {
        let doc = Document::parse("sample", SAMPLE);
        let text = doc.text_of(doc.section("usage").unwrap());
        assert!(
            text.starts_with("## Usage\n") && text.contains("More."),
            "{text}"
        );
        assert!(!text.contains("Again."), "{text}");
    }

    #[test]
    fn search_ranks_a_heading_over_the_body() {
        let doc = Document::parse("sample", SAMPLE);
        let out = search(Target::whole(&doc).sections(), "detail").unwrap();
        assert!(out.starts_with("sample#detail  Title › Detail\n"), "{out}");
    }

    /// A target narrows the search to its section and the subsections under it.
    #[test]
    fn search_within_a_section_spans_its_subsections() {
        let docs = [Document::parse("sample", SAMPLE)];
        let usage = resolve(&docs, "sample#usage").unwrap();
        assert!(
            search(usage.sections(), "more")
                .unwrap()
                .starts_with("sample#detail ")
        );
        assert!(search(usage.sections(), "again").is_err());
    }

    #[test]
    fn search_finds_let_else_in_the_bundle() {
        let docs = documents();
        let all = docs.iter().flat_map(|doc| Target::whole(doc).sections());
        let out = search(all, "let else").unwrap();
        assert!(
            out.lines()
                .next()
                .unwrap()
                .starts_with("control-flow#let-else"),
            "{out}"
        );
    }

    #[test]
    fn a_miss_points_at_doc_search() {
        let doc = Document::parse("sample", SAMPLE);
        let err = search(Target::whole(&doc).sections(), "zzz").unwrap_err();
        assert!(err.message.contains("wado doc --search"), "{}", err.message);
    }

    #[test]
    fn an_unknown_section_is_an_error() {
        let docs = documents();
        assert!(resolve(&docs, "effects#nope").is_err());
        assert!(resolve(&docs, "nope").is_err());
        assert!(resolve(&docs, "effects").unwrap().text().starts_with("# "));
    }
}
