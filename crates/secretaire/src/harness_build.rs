use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use anyhow::{bail, ensure, Context, Result};
use fuzukue_cards::latex_book::Engine;
use fuzukue_cards::{workspace_paths, Card, CardPart, DeckConfig, Graph, TexTarget};
use regex::{Captures, Regex};
use serde::{Deserialize, Serialize};
use serde_yaml::Value;

use super::{BuildLog, LatexMode, CLEVEREF, MATH};

#[path = "harness_html.rs"]
mod html;

static REFERENCES: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"\[\[([^\]|\r\n]+)(?:\|(?P<caption>[^\]\r\n]*))?\]\]|",
        r"`(?P<code>[^`\r\n]+)`|",
        r"\\(?:[Cc]ref|(?:auto|eq|page)?ref|label)\*?[ \t]*\{[^}\r\n]*\}|",
        r"\\[A-Za-z]+|[A-Za-z0-9_:-]+"
    ))
    .unwrap()
});
static STEP: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\((\d+)\)[ \t]*").unwrap());
static STEP_NUMBER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\((\d+)\)").unwrap());
static EXTERNAL_STEP: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"([a-z][a-z0-9-]*)([ \t]*(?:の証明|proof)[ \t]*)\((\d+)\)").unwrap()
});
static MATH_SPAN: LazyLock<Regex> = LazyLock::new(|| {
    let environments = ["equation", "align", "gather", "multline"]
        .map(|env| format!(r"\\begin\{{{env}\*?\}}.*?\\end\{{{env}\*?\}}"));
    Regex::new(&format!(
        r"(?s)\$\$.*?\$\$|\$[^$]*\$|\\\[.*?\\\]|\\\(.*?\\\)|{}",
        environments.join("|")
    ))
    .unwrap()
});

#[derive(Serialize, Deserialize)]
pub(crate) struct Source {
    pub card: String,
    pub path: PathBuf,
    pub lines: Vec<u64>,
}

pub(crate) type SourceMap = BTreeMap<PathBuf, Source>;

pub(crate) fn map_path(pdf: &Path) -> PathBuf {
    pdf.with_extension("sources.json")
}

struct PartFile {
    tex: String,
    source: Source,
}

struct Renderer<'a> {
    graph: &'a Graph,
    command: &'a str,
    language: &'a str,
    references: BTreeMap<String, String>,
    stepped: BTreeMap<&'a str, BTreeSet<&'a str>>,
    author_proofs: BTreeMap<&'a str, &'a Card>,
    parts: BTreeMap<PathBuf, PartFile>,
}

fn label(card: &Card) -> String {
    card.label
        .as_ref()
        .filter(|label| !label.is_empty())
        .cloned()
        .unwrap_or_else(|| format!("card:{}", card.id))
}

fn tex_code(text: &str) -> String {
    format!("\\texttt{{{}}}", tex_literal(text))
}

fn tex_literal(text: &str) -> String {
    let mut tex = String::new();
    for c in text.chars() {
        match c {
            '\\' => tex.push_str(r"\textbackslash{}"),
            '^' => tex.push_str(r"\textasciicircum{}"),
            '~' => tex.push_str(r"\textasciitilde{}"),
            '#' | '$' | '%' | '&' | '_' | '{' | '}' => {
                tex.push('\\');
                tex.push(c);
            }
            _ => tex.push(c),
        }
    }
    tex
}

fn environment(kind: &str) -> Option<&str> {
    match kind {
        "introthm" | "introdef" | "question" => Some(kind),
        kind => super::env_of(kind),
    }
}

pub(crate) fn item_card(item: &Value) -> Option<&str> {
    item.as_str()
        .or_else(|| item["proof_of"].as_str())
        .or_else(|| item["card"].as_str())
}

fn heading(command: &str, title: &str, label: &Value) -> String {
    let label = label
        .as_str()
        .filter(|label| !label.is_empty())
        .map(|label| format!("\\label{{{label}}}"))
        .unwrap_or_default();
    format!("\\{command}{{{title}}}{label}\n")
}

fn outside_math(proof: &str) -> impl Iterator<Item = (usize, &str)> {
    let mut start = 0;
    MATH_SPAN
        .find_iter(proof)
        .map(Some)
        .chain([None])
        .map(move |math| {
            let end = math.as_ref().map_or(proof.len(), |math| math.start());
            let span = (start, &proof[start..end]);
            start = math.map_or(end, |math| math.end());
            span
        })
}

fn proof_steps(graph: &Graph) -> BTreeMap<&str, BTreeSet<&str>> {
    let mut external = BTreeMap::<_, BTreeSet<_>>::new();
    for card in graph.cards.values().filter(|card| !card.external) {
        for cap in EXTERNAL_STEP.captures_iter(&card.body) {
            external
                .entry(cap.get(1).unwrap().as_str())
                .or_default()
                .insert(cap.get(3).unwrap().as_str());
        }
    }
    graph
        .cards
        .values()
        .filter_map(|card| {
            let proof = &card.proof.as_ref()?.text;
            let steps: BTreeSet<_> = proof
                .lines()
                .filter_map(|line| STEP.captures(line))
                .map(|cap| cap.get(1).unwrap().as_str())
                .collect();
            let internal = outside_math(proof).any(|(offset, span)| {
                STEP_NUMBER.captures_iter(span).any(|cap| {
                    let pos = offset + cap.get(0).unwrap().start();
                    pos > 0 && proof.as_bytes()[pos - 1] != b'\n' && steps.contains(&cap[1])
                })
            });
            let cited = external
                .get(card.id.as_str())
                .is_some_and(|refs| !steps.is_disjoint(refs));
            (internal || cited).then_some((card.id.as_str(), steps))
        })
        .collect()
}

impl<'a> Renderer<'a> {
    fn new(graph: &'a Graph, command: &'a str, language: &'a str) -> Self {
        let mut references = BTreeMap::new();
        let mut author_proofs = BTreeMap::new();
        for card in graph.cards.values().filter(|card| !card.external) {
            if card.kind == "explanation" {
                if card.role.as_deref() == Some("proof") {
                    for id in &card.explains {
                        author_proofs.insert(id.as_str(), card);
                    }
                }
            } else if environment(&card.kind).is_some() {
                let label = label(card);
                references.insert(card.id.clone(), label.clone());
                references.insert(label.clone(), label);
            }
        }
        Self {
            graph,
            command,
            language,
            references,
            stepped: proof_steps(graph),
            author_proofs,
            parts: BTreeMap::new(),
        }
    }

    fn link_refs(&self, text: &str, bare: bool) -> String {
        REFERENCES
            .replace_all(text, |cap: &Captures<'_>| {
                let matched = cap.get(0).unwrap();
                if let Some(code) = cap.name("code") {
                    return tex_code(code.as_str());
                }
                let key = match cap.get(1) {
                    Some(link) => link.as_str(),
                    None if bare
                        && !matched.as_str().starts_with('\\')
                        && !text[..matched.start()].ends_with('{')
                        && !text[matched.end()..].starts_with('}') =>
                    {
                        matched.as_str()
                    }
                    None => return matched.as_str().to_owned(),
                };
                self.references
                    .get(key)
                    .map(|label| format!("\\{}{{{label}}}", self.command))
                    .or_else(|| {
                        let card = self.graph.cards.get(key).filter(|card| card.external)?;
                        cap.get(1)?;
                        Some(cap.name("caption").map_or_else(
                            || tex_code(&card.id),
                            |caption| tex_literal(caption.as_str()),
                        ))
                    })
                    .unwrap_or_else(|| matched.as_str().to_owned())
            })
            .into_owned()
    }

    fn external_steps(&self, text: &str) -> String {
        EXTERNAL_STEP
            .replace_all(text, |cap: &Captures<'_>| {
                if !self.stepped.contains_key(&cap[1]) {
                    return cap[0].to_owned();
                }
                let mid = cap[2].trim_end();
                let join = if mid.contains("の証明") {
                    "の"
                } else {
                    " "
                };
                format!("{}{mid}{join}Step {}", &cap[1], &cap[3])
            })
            .into_owned()
    }

    fn fact_proof(&self, card: &Card, part: &CardPart) -> (String, Vec<u64>) {
        let mut body = String::new();
        let mut lines = Vec::new();
        let stepped = self.stepped.get(card.id.as_str());
        let mut proof = part.text.clone();
        if let Some(steps) = stepped {
            // Replace cited step numbers outside math while keeping all original newlines.
            let mut formatted = String::new();
            let mut end = 0;
            for (offset, span) in outside_math(&proof) {
                formatted.push_str(&proof[end..offset]);
                let mut pos = 0;
                for cap in STEP_NUMBER.captures_iter(span) {
                    let found = cap.get(0).unwrap();
                    let at = offset + found.start();
                    if at > 0 && proof.as_bytes()[at - 1] != b'\n' && steps.contains(&cap[1]) {
                        formatted.push_str(&span[pos..found.start()]);
                        write!(formatted, "Step {}", &cap[1]).unwrap();
                        pos = found.end();
                    }
                }
                formatted.push_str(&span[pos..]);
                end = offset + span.len();
            }
            proof = formatted;
        }
        for (index, line) in proof.lines().enumerate() {
            let original = part.line + index as u64;
            if let Some(step) = STEP.captures(line) {
                if index > 0 {
                    body.push('\n');
                    lines.push(original);
                }
                if stepped.is_some() {
                    write!(body, "\\emph{{Step {}.}} ", &step[1]).unwrap();
                }
                body.push_str(&line[step.get(0).unwrap().end()..]);
            } else {
                body.push_str(line);
            }
            body.push('\n');
            lines.push(original);
        }
        (self.external_steps(&body), lines)
    }

    fn input(&mut self, card: &Card, suffix: &str, tex: String, lines: Vec<u64>) -> Result<String> {
        ensure!(
            card.id
                .as_bytes()
                .first()
                .is_some_and(u8::is_ascii_lowercase)
                && card
                    .id
                    .bytes()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-'),
            "ハーネスカードの ID が不正: {}",
            card.id
        );
        let path = PathBuf::from(format!("cards/{}.{suffix}.tex", card.id));
        let source = Source {
            card: card.id.clone(),
            path: card.path.clone(),
            lines,
        };
        self.parts.insert(path.clone(), PartFile { tex, source });
        // The last line of the input already supplies its own endline.
        Ok(format!("\\input{{{}}}%\n", path.to_string_lossy()))
    }

    fn statement(&mut self, card: &Card) -> Result<String> {
        if matches!(card.kind.as_str(), "explanation" | "expl") {
            if card.role.as_deref() == Some("proof") {
                return Ok(String::new());
            }
            if let Some(input) = super::card_input(card, "statement") {
                return Ok(input);
            }
            let Some(prose) = card.prose.as_ref().or(card.statement.as_ref()) else {
                return Ok(String::new());
            };
            return self.input(
                card,
                "statement",
                format!("{}\n", self.link_refs(&prose.text, false)),
                source_lines(prose),
            );
        }
        let env = environment(&card.kind)
            .with_context(|| format!("カード {} の種類が不明: {}", card.id, card.kind))?;
        let title = if card.title.is_empty() {
            String::new()
        } else {
            format!("[{{{}}}]", card.title)
        };
        let empty = CardPart {
            text: String::new(),
            line: 1,
        };
        let part = card.statement.as_ref().unwrap_or(&empty);
        let label = label(card);
        let start = format!("\\begin{{{env}}}{title}\\label{{{label}}}\n");
        if let Some(input) = super::card_input(card, "statement") {
            return Ok(format!("{start}{input}\\end{{{env}}}\n"));
        }
        let body = self.link_refs(&self.external_steps(&part.text), true);
        let lines = source_lines(part);
        let mapped = wrap_lines(&start, lines, part.line);
        self.input(
            card,
            "statement",
            format!("{start}{body}\n\\end{{{env}}}\n"),
            mapped,
        )
    }

    fn proof(
        &mut self,
        card: &'a Card,
        author: Option<&'a Card>,
        deferred: bool,
    ) -> Result<String> {
        let source = author.unwrap_or(card);
        let part = if author.is_some() {
            source.prose.as_ref()
        } else {
            source.proof.as_ref()
        };
        let Some(part) = part.filter(|part| !part.text.is_empty()) else {
            return Ok(String::new());
        };
        let (body, lines) = if author.is_some() {
            (
                format!("{}\n", self.link_refs(&part.text, false)),
                source_lines(part),
            )
        } else {
            let (body, lines) = self.fact_proof(card, part);
            (self.link_refs(&body, true), lines)
        };
        let mut begin = "\\begin{proof}".to_owned();
        if deferred {
            let label = label(card);
            let head = if self.language == "ja" && self.command != "ref" {
                format!("\\{}{{{label}}}の証明", self.command)
            } else if self.language == "ja" {
                let kind = match card.kind.as_str() {
                    "theorem" => "定理",
                    "proposition" => "命題",
                    "lemma" => "補題",
                    "corollary" => "系",
                    _ => "",
                };
                format!("{kind}\\ref{{{label}}}の証明")
            } else {
                let mut kind = card.kind.clone();
                kind.get_mut(..1).map(str::make_ascii_uppercase);
                format!("Proof of {kind}~\\ref{{{label}}}")
            };
            write!(begin, "[{{{head}}}]").unwrap();
        }
        if let Some(input) = super::card_input(source, "proof") {
            return Ok(format!("{begin}\n{input}\\end{{proof}}\n"));
        }
        let suffix = if deferred {
            format!("{}.deferred-proof", card.id)
        } else {
            "proof".to_owned()
        };
        let lines = wrap_lines(&begin, lines, part.line);
        let input = self.input(
            source,
            &suffix,
            format!("{begin}\n{body}\\end{{proof}}\n"),
            lines,
        )?;
        Ok(input)
    }

    fn item(&mut self, item: &Value) -> Result<Option<String>> {
        if let Some(title) = item["subsection"].as_str() {
            return Ok(Some(heading("subsection", title, &item["label"])));
        }
        let Some(card) = item_card(item).and_then(|id| self.graph.cards.get(id)) else {
            return Ok(None);
        };
        let author = self.author_proofs.get(card.id.as_str()).copied();
        if item["proof_of"].is_string() {
            return self.proof(card, author, true).map(Some);
        }
        let mut tex = self.statement(card)?;
        if matches!(
            card.kind.as_str(),
            "theorem" | "proposition" | "lemma" | "corollary" | "introthm"
        ) {
            match item["proof"]
                .as_str()
                .unwrap_or(if author.is_some() { "author" } else { "fact" })
            {
                "fact" => tex.push_str(&self.proof(card, None, false)?),
                "author" => tex.push_str(&self.proof(card, author, false)?),
                "appendix" => tex.push_str("\\noindent\\emph{Proof in the appendix.}\n"),
                "omit" | "later" => (),
                mode => bail!("カード {} の proof が不明: {mode}", card.id),
            }
        }
        Ok(Some(tex))
    }

    fn document(
        &mut self,
        preamble: &str,
        structure: &Value,
        sections: Option<usize>,
        appendix: &str,
        bib: &str,
    ) -> Result<String> {
        let mut tex = format!("{preamble}\\begin{{document}}\n\\maketitle\n\n");
        let mut blocks = Vec::new();
        for section in sequence(&structure["sections"]).take(sections.unwrap_or(usize::MAX)) {
            let mut items = sequence(&section["items"])
                .filter_map(|item| self.item(item).transpose())
                .peekable();
            if items.peek().is_some() {
                blocks.push(heading(
                    "section",
                    section["title"].as_str().unwrap_or(""),
                    &section["label"],
                ));
                blocks.extend(items.collect::<Result<Vec<_>>>()?);
            }
        }
        if sections.is_none() {
            let mut items = sequence(&structure["appendix"])
                .filter_map(|item| self.item(item).transpose())
                .peekable();
            if items.peek().is_some() {
                blocks.push(format!("\\appendix\n\\section{{{appendix}}}\n"));
                blocks.extend(items.collect::<Result<Vec<_>>>()?);
            }
        }
        tex.push_str(&blocks.join("\n"));
        tex.push_str(bib);
        tex.push_str("\\end{document}\n");
        Ok(tex)
    }
}

fn sequence(value: &Value) -> impl Iterator<Item = &Value> {
    value.as_sequence().into_iter().flatten()
}

fn validate<'a>(graph: &Graph, structure: &'a Value, path: &Path) -> Result<()> {
    ensure!(
        structure.is_mapping(),
        "{}: expected an outline",
        path.display()
    );
    ensure!(
        structure["title"].is_null() || structure["title"].is_string(),
        "{}: title must be a string",
        path.display()
    );
    let sections = structure["sections"]
        .as_sequence()
        .context("sections must be a list")?;
    let mut statements = BTreeSet::new();
    let mut deferred = BTreeSet::new();
    let mut proofs = BTreeSet::new();
    let mut check_items = |items: &'a Value| -> Result<()> {
        for item in items
            .as_sequence()
            .context("outline items must be a list")?
        {
            if let Some(fields) = item.as_mapping() {
                let keys = ["card", "proof_of", "subsection"];
                ensure!(
                    keys.iter().filter(|&&key| fields.contains_key(key)).count() == 1,
                    "outline item needs one of card, proof_of, or subsection: {item:?}"
                );
                for key in ["card", "proof_of", "subsection", "label", "proof"] {
                    ensure!(
                        !fields.contains_key(key) || item[key].is_string(),
                        "outline item {key} must be a string: {item:?}"
                    );
                }
                ensure!(
                    item["proof"].is_null() || item["card"].is_string(),
                    "proof mode requires a card item: {item:?}"
                );
                if item["subsection"].is_string() {
                    continue;
                }
            }
            let id = item_card(item).with_context(|| format!("invalid outline item: {item:?}"))?;
            let card = graph
                .cards
                .get(id)
                .filter(|card| !card.external)
                .with_context(|| format!("outline references unknown local card `{id}`"))?;
            if item["proof_of"].is_string() {
                ensure!(
                    deferred.contains(id),
                    "proof of `{id}` needs an earlier statement with proof: later"
                );
                ensure!(proofs.insert(id), "proof of `{id}` appears twice");
                continue;
            }
            ensure!(statements.insert(id), "outline card `{id}` appears twice");
            ensure!(
                environment(&card.kind).is_some()
                    || matches!(card.kind.as_str(), "explanation" | "expl"),
                "outline card `{id}` has unknown kind `{}`",
                card.kind
            );
            match item["proof"].as_str() {
                Some("later") => {
                    deferred.insert(id);
                }
                Some("fact" | "author" | "appendix" | "omit") | None => (),
                Some(mode) => bail!("outline card `{id}` has unknown proof mode `{mode}`"),
            }
        }
        Ok(())
    };
    for section in sections {
        ensure!(
            section["title"].is_string(),
            "outline section needs a title"
        );
        ensure!(
            section["label"].is_null() || section["label"].is_string(),
            "outline section label must be a string"
        );
        check_items(&section["items"])?;
    }
    if !structure["appendix"].is_null() {
        check_items(&structure["appendix"])?;
    }
    if let Some(id) = deferred.difference(&proofs).next() {
        bail!("outline card `{id}` has proof: later without a proof_of item");
    }
    for card in graph.cards.values().filter(|card| !card.external) {
        for dependency in card.links.iter().filter(|link| link.rel == "requires") {
            ensure!(
                graph.cards.contains_key(&dependency.to),
                "card `{}` requires unknown card `{}`",
                card.id,
                dependency.to
            );
        }
    }
    Ok(())
}

fn source_lines(part: &CardPart) -> Vec<u64> {
    (part.line..)
        .take(part.text.lines().count().max(1))
        .collect()
}

fn wrap_lines(begin: &str, mut lines: Vec<u64>, first: u64) -> Vec<u64> {
    let last = lines.last().copied().unwrap_or(first);
    lines.splice(
        ..0,
        std::iter::repeat_n(first, begin.trim_end_matches('\n').lines().count()),
    );
    lines.push(last);
    lines
}

const THEOREMS: &str = r"\theoremstyle{plain}
\newtheorem{theorem}{Theorem}[section]
\newtheorem{proposition}[theorem]{Proposition}
\newtheorem{lemma}[theorem]{Lemma}
\newtheorem{corollary}[theorem]{Corollary}
\theoremstyle{definition}
\newtheorem{definition}[theorem]{Definition}
\theoremstyle{remark}
\newtheorem{remark}[theorem]{Remark}
";

fn preamble(
    workspace: &Path,
    deck: &Path,
    cfg: &DeckConfig,
    title: &str,
    language: &str,
) -> Result<String> {
    let mut tex = if let Some(path) = &cfg.meta.preamble {
        std::fs::read_to_string(workspace_paths::resolve(workspace, deck, path)?)
            .with_context(|| format!("{} を読めない", path.display()))?
    } else {
        let mut tex = match language {
            "ja" => format!("% !TeX program = uplatex\n{}{MATH}{}", super::ARTICLE, super::LINKS),
            "en" => format!("% !TeX program = pdflatex\n\\documentclass[11pt]{{amsart}}\n{MATH}\\usepackage[hidelinks]{{hyperref}}\n"),
            _ => bail!("language = {language:?} には preamble が必要"),
        };
        tex.push_str(CLEVEREF);
        tex.push_str(THEOREMS);
        let default_macros = Path::new("macros.tex");
        if let Some(path) = &cfg.macros_file {
            tex.push_str(&std::fs::read_to_string(workspace_paths::resolve(
                workspace, deck, path,
            )?)?);
        } else if deck.join(default_macros).is_file() {
            tex.push_str(&std::fs::read_to_string(workspace_paths::resolve(
                workspace,
                deck,
                default_macros,
            )?)?);
        }
        tex.push_str("\n\\date{\\today}\n");
        tex
    };
    if !tex.ends_with('\n') {
        tex.push('\n');
    }
    for (command, argument) in [
        ("title", title),
        ("author", cfg.meta.author.as_deref().unwrap_or("")),
    ] {
        let defined = Regex::new(&format!(r"\\{command}\s*(?:\[|\{{)")).unwrap();
        if !tex
            .lines()
            .any(|line| defined.is_match(line.split('%').next().unwrap_or(line)))
        {
            writeln!(tex, "\\{command}{{{argument}}}").unwrap();
        }
    }
    Ok(tex)
}

fn detect_engine(preamble: &str, language: &str) -> Result<Engine> {
    static PROGRAM: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?im)^\s*%\s*!\s*TeX\s+(?:TS-)?program\s*=\s*([A-Za-z]+)").unwrap()
    });
    let engine = PROGRAM
        .captures(preamble)
        .map(|cap| cap[1].to_ascii_lowercase());
    Ok(match engine.as_deref() {
        Some("uplatex") => Engine::UpLaTeX,
        Some("platex") => Engine::PLaTeX,
        Some("pdflatex") => Engine::PdfLaTeX,
        Some("lualatex") => Engine::LuaLaTeX,
        Some("xelatex") => Engine::XeLaTeX,
        Some(engine) => bail!("TeX program に対応していない: {engine}"),
        None if language == "ja" => Engine::UpLaTeX,
        None => Engine::PdfLaTeX,
    })
}

fn bibliography(workspace: &Path, deck: &Path, cfg: &DeckConfig, relative: bool) -> Result<String> {
    let Some(bib) = &cfg.meta.bibliography else {
        return Ok(String::new());
    };
    let absolute = workspace_paths::resolve(workspace, deck, bib)?;
    ensure!(absolute.is_file(), "{} がない", absolute.display());
    let path = if relative {
        workspace_paths::relative(&deck.join("build"), &absolute)?
    } else {
        absolute
    };
    Ok(format!(
        "\n\\bibliographystyle{{amsplain}}\n\\bibliography{{{}}}\n",
        path
            .with_extension("")
            .to_string_lossy()
            .replace('\\', "/")
    ))
}

pub(crate) fn targets(cfg: &DeckConfig) -> Result<Vec<(&str, Option<usize>)>> {
    let targets = cfg
        .meta
        .build
        .as_ref()
        .map(|build| build.tex.as_slice())
        .unwrap_or_default();
    if targets.is_empty() {
        return Ok(vec![("cards.tex", None)]);
    }
    let mut names = BTreeSet::new();
    targets
        .iter()
        .map(|target| {
            let (name, sections) = match target {
                TexTarget::File(name) => (name.as_str(), None),
                TexTarget::Part { out, sections } => (out.as_str(), *sections),
            };
            ensure!(
                !name.starts_with(['.', '-'])
                    && name.ends_with(".tex")
                    && name
                        .bytes()
                        .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c)),
                "[build].tex のファイル名が不正: {name}"
            );
            ensure!(names.insert(name), "[build].tex のファイル名が重複: {name}");
            Ok((name, sections))
        })
        .collect()
}

fn write_generated(path: &Path, bytes: &[u8]) -> Result<()> {
    if std::fs::read(path).is_ok_and(|old| old == bytes) {
        return Ok(());
    }
    let mut temp =
        tempfile::NamedTempFile::new_in(path.parent().context("build path has no parent")?)?;
    std::io::Write::write_all(&mut temp, bytes)?;
    temp.persist(path)?;
    Ok(())
}

pub(super) async fn build(
    workspace: &Path,
    deck: &Path,
    deck_rel: &str,
    cfg: &DeckConfig,
    graph: &Graph,
    cancel: &crate::cancellation::Cancellation,
    log: &mut BuildLog,
) -> Result<super::BuildOutputs> {
    workspace_paths::resolve(workspace, deck, &cfg.structure)?;
    let structure = super::read_structure(deck, cfg)?;
    validate(graph, &structure, &cfg.structure)
        .with_context(|| format!("invalid harness: {}", cfg.structure.display()))?;
    let title = structure["title"]
        .as_str()
        .filter(|t| !t.is_empty())
        .or_else(|| (!cfg.meta.title.is_empty()).then_some(cfg.meta.title.as_str()))
        .unwrap_or("Draft");
    let language = cfg.meta.language.as_deref().unwrap_or("en");
    let preamble = preamble(workspace, deck, cfg, title, language)?;
    let settings = cfg.meta.build.as_ref();
    let extra = settings
        .map(|build| build.latexmk_extra.as_slice())
        .unwrap_or_default();
    let engine = match settings.and_then(|build| build.engine) {
        Some(engine) => engine,
        None => detect_engine(&preamble, language)?,
    };
    log.engine = Some(engine.name());
    let bib = bibliography(workspace, deck, cfg, log.diagnostics.is_some())?;
    let targets = targets(cfg)?;
    let out = deck.join("build");
    ensure!(
        !out.is_symlink() && !out.join("cards").is_symlink(),
        "build/ と build/cards/ には実ディレクトリが必要"
    );
    let appendix = cfg
        .meta
        .appendix_title
        .as_deref()
        .unwrap_or("Deferred proofs");
    let mut render = Renderer::new(
        graph,
        cfg.meta.ref_command.as_deref().unwrap_or("ref"),
        language,
    );
    let mut rendered = Vec::new();
    for (name, sections) in &targets {
        cancel.check()?;
        let tex = render.document(&preamble, &structure, *sections, appendix, &bib)?;
        rendered.push((*name, tex, std::mem::take(&mut render.parts)));
    }
    let html = html::export(workspace, deck, cfg, &render, &structure, title, appendix)?;
    for dir in [&out, &out.join("cards")] {
        if dir.is_dir() {
            for entry in std::fs::read_dir(dir)? {
                let entry = entry?;
                workspace_paths::resolve(workspace, deck, &entry.path())?;
            }
        }
    }
    std::fs::create_dir_all(out.join("cards"))?;
    let mut ok = true;
    for (name, tex, parts) in rendered {
        cancel.check()?;
        let mut map = SourceMap::new();
        for (path, part) in parts {
            write_generated(&out.join(&path), part.tex.as_bytes())?;
            map.insert(path, part.source);
        }
        write_generated(&out.join(name), tex.as_bytes())?;
        let source = Path::new(name);
        let mut command = super::latexmk_command(&out, source, &LatexMode::Harness(engine), extra);
        log.start(&Path::new(deck_rel).join("build").join(source), engine);
        let template = out.join(name);
        let log_path = template.with_extension("log");
        let has_synctex = || {
            ["synctex.gz", "synctex"].into_iter().any(|ext| {
                template
                    .with_extension(ext)
                    .metadata()
                    .is_ok_and(|meta| meta.len() > 0)
            })
        };
        let mut run = log.run(&mut command, &log_path, cancel).await?;
        if run.status.success() && !has_synctex() {
            run = log.run(command.arg("-g"), &log_path, cancel).await?;
        }
        log.diagnose(&run, &log_path);
        let synced = run.status.success() && has_synctex();
        ok &= synced;
        if synced {
            write_generated(&map_path(&template), &serde_json::to_vec(&map)?)?;
        } else if run.status.success() {
            log.line(format_args!("{name}: SyncTeX が生成されなかった"));
            if let Some(diagnostics) = &mut log.diagnostics {
                diagnostics.error(format!("{name}: SyncTeX が生成されなかった"));
            }
        }
    }
    cancel.check()?;
    for (path, content) in html {
        std::fs::create_dir_all(path.parent().context("HTML output has no parent")?)?;
        write_generated(&path, content.as_bytes())?;
    }
    let pdf_path = |name| Path::new("build").join(name).with_extension("pdf");
    Ok(super::BuildOutputs {
        ok,
        pdf: pdf_path(targets[0].0),
        additional_pdfs: targets[1..].iter().map(|(name, _)| pdf_path(name)).collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestDir;
    use fuzukue_proto::{ClientMsg, ServerEvent};

    const DECK: &str = "papers/relocated/harness-en";
    const EXTERNAL: &str = "papers/sample-paper/harness";
    const SITE: &str = "website/private/cards";
    const HTML_OUT: &str = "website/private/cards/relocated";
    const LOCAL_CARD: &str = r#"---
id: result
kind: theorem
title: Fixture result
requires: [ctg:source]
---
## Statement
See [[ctg:source|external source]]. For every $x < y$, $x < y$.

## Proof
Immediate. The source filename is `notes_old.tex`.
"#;
    const OUTLINE: &str = r#"title: Relocated fixture
sections:
  - title: Results
    items:
      - {subsection: Main result}
      - {card: result, proof: later}
  - title: Proofs
    items: [{proof_of: result}]
"#;
    const CONFIG: &str = r#"path_base = "workspace"
language = "en"
bibliography = ""
html_out = "website/private/cards/relocated"
mathjax = "../vendor/mathjax.js"
[[html_links]]
text = "All papers"
href = "../index.html"
[html_index]
title = "Local papers"
links = [{text = "Home", href = "../home.html"}]
[mathjax_macros]
pair = ['\left(#1,#2\right)', 2]
[external_harnesses]
ctg = "papers/sample-paper/harness"
"#;

    fn fixture() -> TestDir {
        let dir = TestDir::new();
        dir.write("fuzukue.toml", "title = 'Lab'\n");
        dir.write(&format!("{DECK}/harness.toml"), CONFIG);
        dir.write(&format!("{DECK}/structure.yaml"), OUTLINE);
        dir.write(&format!("{DECK}/cards/result.md"), LOCAL_CARD);
        dir.write(
            &format!("{DECK}/cards/unused.md"),
            "---\nid: unused\nkind: lemma\n---\n## Statement\nUnused in the paper.\n",
        );
        dir.write(
            &format!("{EXTERNAL}/harness.toml"),
            r#"path_base = "workspace"
language = "en"
cards_dir = "facts"
bibliography = ""
html_out = "website/private/cards/sample-paper"
"#,
        );
        dir.write(
            &format!("{EXTERNAL}/structure.yaml"),
            "title: Compact target fixture\nsections: [{title: Definitions, items: [source]}]\n",
        );
        dir.write(
            &format!("{EXTERNAL}/facts/source.md"),
            "---\nid: source\nkind: definition\n---\n## Statement\nExternal definition.\n",
        );
        dir.write(
            &format!("{SITE}/vendor/mathjax.js"),
            "// local MathJax script URL",
        );
        dir.write(
            &format!("{SITE}/other paper/index.html"),
            "<h1>Sibling viewer</h1>",
        );
        dir.write("website/private/home.html", "<h1>Private site home</h1>");
        dir
    }

    #[tokio::test]
    async fn relocated_harness_build_paper_exports_pdf_tex_and_html() -> Result<()> {
        let dir = fixture();
        let moved = TestDir::new();
        let workspace = moved.path().join("relocated workspace");
        std::fs::rename(dir.path(), &workspace)?;
        let deck = workspace.join(DECK);
        assert!(!workspace.join(".agents").exists());
        let graph = Graph::open_in_project(&deck, &[], Some(&workspace))?;
        assert!(graph.cards["ctg:source"].external);
        assert_eq!(
            deck.join(&graph.cards["ctg:source"].path).canonicalize()?,
            workspace.join(EXTERNAL).join("facts/source.md")
        );

        for tool in ["latexmk", "pdflatex"] {
            if let Err(error) = std::process::Command::new(tool).arg("--version").output() {
                if error.kind() == std::io::ErrorKind::NotFound {
                    eprintln!("skipping harness PDF fixture; missing {tool}");
                    return Ok(());
                }
                return Err(error.into());
            }
        }
        let app = std::sync::Arc::new(crate::Workspace::new(
            workspace.clone(),
            String::new(),
            workspace.join(".fuzukue"),
            std::sync::Arc::new(crate::hub::Hub::new()),
        ));
        let built = app.build_paper(EXTERNAL.into()).await??;
        assert!(built.ok, "{}", built.log);
        assert_eq!(built.pdf, format!("{EXTERNAL}/build/cards.pdf"));
        assert!(std::fs::read(workspace.join(&built.pdf))?.starts_with(b"%PDF-"));
        let tex_root = format!("{EXTERNAL}/build/cards.tex");
        assert_eq!(built.tex_root.as_deref(), Some(tex_root.as_str()));
        let source_tex = std::fs::read_to_string(workspace.join(tex_root))?;
        assert!(source_tex.contains(r"\input{cards/source.statement.tex}"));
        let source_viewer = workspace.join(SITE).join("sample-paper");
        for page in ["index.html", "read.html"] {
            let html = std::fs::read_to_string(source_viewer.join(page))?;
            assert!(html.contains("External definition."));
        }

        let (_, mut events) = app.hub.subscribe(0);
        let deck_selected = ClientMsg::SelectDeck { deck: DECK.into() };
        crate::handle(&app, deck_selected).await?;
        assert!(matches!(events.try_recv()?.event, ServerEvent::Workspace { tabs, deck }
            if deck == DECK && [DECK, EXTERNAL].iter().all(|path|
                tabs.iter().any(|tab| tab.deck.as_deref() == Some(path) && tab.kind == "paper"))));
        assert!(matches!(events.try_recv()?.event, ServerEvent::Cards { deck, cards, .. }
            if deck == DECK && cards.iter().any(|card| card.id == "ctg:source"
                && card.external && card.body.contains("External definition."))));

        crate::handle(&app, ClientMsg::BuildPaper { deck: DECK.into() }).await?;
        let pdf = tokio::time::timeout(std::time::Duration::from_secs(30), async {
            loop {
                if let ServerEvent::BuildResult { deck, ok, pdf, log, .. } = events.recv().await?.event {
                    assert_eq!(deck, DECK);
                    assert!(ok, "{log}");
                    return Ok::<_, anyhow::Error>(pdf);
                }
            }
        })
        .await??;
        assert_eq!(pdf, format!("{DECK}/build/cards.pdf"));
        assert!(std::fs::read(workspace.join(&pdf))?.starts_with(b"%PDF-"));
        assert!(matches!(crate::cards::artifacts(&workspace), ServerEvent::Artifacts { pdfs }
            if pdfs.contains(&built.pdf) && pdfs.contains(&pdf)));
        let tex = std::fs::read_to_string(deck.join("build/cards.tex"))?;
        assert!(tex.contains(r"\input{cards/result.statement.tex}"));
        assert!(
            !tex.contains("unused") && !tex.contains("ctg:source") && !tex.contains("bibliography")
        );
        assert!(!deck.join("build/cards/unused.statement.tex").exists());
        let proof =
            std::fs::read_to_string(deck.join("build/cards/result.result.deferred-proof.tex"))?;
        assert!(proof.contains(r"\texttt{notes\_old.tex}"));
        let site = workspace.join(SITE);
        let viewer = workspace.join(HTML_OUT);
        let index = std::fs::read_to_string(viewer.join("index.html"))?;
        let read = std::fs::read_to_string(viewer.join("read.html"))?;
        let parent = std::fs::read_to_string(site.join("index.html"))?;
        for page in [&index, &read] {
            assert!(page.contains("Relocated fixture"));
            assert!(page.contains(r#"src="../vendor/mathjax.js""#));
            assert!(page.contains(r#""pair":["\\left(#1,#2\\right)",2]"#));
            assert!(page.contains("$x &lt; y$"));
            assert!(!page.contains(&*workspace.to_string_lossy()));
        }
        assert!(index.contains(r#"href="../index.html""#));
        assert!(read.contains(r#"href="index.html""#));
        assert!(index.contains(r#"href="read.html""#));
        assert!(index.contains(r##"href="#ctg:source""##) && index.contains(r#"id="ctg:source""#));
        assert!(index.contains(r#"id="unused""#));
        for href in ["index.html#ctg:source", "#proof:result", "#result"] {
            assert!(read.contains(&format!("href=\"{href}\"")), "missing {href}");
        }
        assert!(!read.contains("Unused in the paper") && !read.contains("External definition"));
        assert!(read.find("Main result").unwrap() < read.find("Immediate.").unwrap());
        assert!(read.contains("<code>notes_old.tex</code>"));
        for href in [
            "relocated/index.html",
            "sample-paper/index.html",
            "other%20paper/index.html",
            "../home.html",
        ] {
            assert!(
                parent.contains(&format!("href=\"{href}\"")),
                "missing {href}"
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn rejects_invalid_harness_before_writing_artifacts() -> Result<()> {
        let dir = fixture();
        let workspace = dir.path();
        let deck = workspace.join(DECK);
        let error = async |message: &str| -> Result<()> {
            let error = crate::build::build(workspace, DECK)
                .await
                .err()
                .context("expected build to fail")?;
            assert!(format!("{error:#}").contains(message), "{error:#}");
            assert!(!deck.join("build").exists());
            assert!(!workspace.join(HTML_OUT).join("index.html").exists());
            Ok(())
        };
        dir.write(
            &format!("{DECK}/structure.yaml"),
            &OUTLINE.replace("card: result", "card: typo"),
        );
        error("unknown local card `typo`").await?;
        dir.write(
            &format!("{DECK}/structure.yaml"),
            &OUTLINE.replace("proof: later", "proof: omit"),
        );
        error("needs an earlier statement").await?;
        dir.write(&format!("{DECK}/structure.yaml"), OUTLINE);
        dir.write(
            &format!("{DECK}/cards/result.md"),
            &LOCAL_CARD.replace("ctg:source", "ctg:missing"),
        );
        error("requires unknown card `ctg:missing`").await?;
        dir.write(&format!("{DECK}/cards/result.md"), LOCAL_CARD);
        for field in [HTML_OUT, EXTERNAL] {
            dir.write(
                &format!("{DECK}/harness.toml"),
                &CONFIG.replace(field, "../outside"),
            );
            error("path escapes workspace").await?;
        }
        #[cfg(unix)]
        {
            let outside = TestDir::new();
            outside.write("index.html", "outside");
            std::os::unix::fs::symlink(outside.path(), workspace.join("escape"))?;
            for field in [HTML_OUT, EXTERNAL] {
                dir.write(
                    &format!("{DECK}/harness.toml"),
                    &CONFIG.replace(field, "escape"),
                );
                error("path escapes workspace").await?;
            }
            dir.write(
                &format!("{DECK}/harness.toml"),
                &CONFIG.replace(EXTERNAL, "escape"),
            );
            let browsing = Graph::open_in_project(&deck, &[], Some(workspace)).unwrap_err();
            assert!(format!("{browsing:#}").contains("path escapes workspace"));
            dir.write(&format!("{DECK}/harness.toml"), CONFIG);
            let viewer = workspace.join(HTML_OUT);
            std::fs::create_dir_all(&viewer)?;
            std::os::unix::fs::symlink(
                outside.path().join("index.html"),
                viewer.join("read.html"),
            )?;
            error("path escapes workspace").await?;
            assert_eq!(
                std::fs::read_to_string(outside.path().join("index.html"))?,
                "outside"
            );
        }
        Ok(())
    }

    #[test]
    fn renders_harness_targets_and_maps_generated_proof_steps() -> Result<()> {
        let deck = tempfile::tempdir()?;
        let root = deck.path();
        std::fs::create_dir_all(root.join("cards/prop"))?;
        std::fs::create_dir(root.join("cards/expl"))?;
        std::fs::write(
            root.join("harness.toml"),
            r#"
language = "ja"
author = "Author Name"
preamble = "preamble.tex"
ref_command = "Cref"
bibliography = "refs.bib"
[build]
tex = ["full.tex", {out = "part1.tex", sections = 1}]
"#,
        )?;
        let preamble_src =
            "% !TeX program = lualatex\n\\documentclass{article}\n% keep this comment  \n";
        std::fs::write(root.join("preamble.tex"), preamble_src)?;
        std::fs::write(root.join("refs.bib"), "")?;
        std::fs::write(
            root.join("cards/prop/first.md"),
            r#"---
id: first
kind: theorem
label: thm:one
title: A title [with brackets]
---
## Statement
See [[second|the lemma]], second and thm:two. Existing \Cref{thm:two}.
## Proof
(1) Begin with second.
(2) Use (1), with $(1)$ unchanged.
## Shortcuts
Ignore this shortcut.
## Notes
Ignore this note.
"#,
        )?;
        std::fs::write(root.join("cards/prop/second.md"), "---\nid: second\nkind: lemma\nlabel: thm:two\n---\n## Statement\nA lemma.\n## Proof\nReference proof.\n")?;
        std::fs::write(root.join("cards/expl/author.md"), "---\nid: author\nkind: explanation\nrole: proof\nexplains: [second]\n---\nAuthor proof of [[first]].\n")?;
        std::fs::write(
            root.join("cards/expl/intro.md"),
            "---\nid: intro\nkind: explanation\n---\n\\begin{introthm}A result.\\end{introthm}\n",
        )?;
        let structure: Value = serde_yaml::from_str(
            r#"
title: Paper title
sections:
- title: Opening
  label: sec:opening
  items:
  - intro
  - {subsection: Results, label: sec:results}
  - {card: first, proof: later}
  - second
- title: Next section
  items:
  - {proof_of: first}
appendix: [{card: second, proof: omit}]
"#,
        )?;
        let cfg = DeckConfig::read(root)?;
        assert!(cfg.harness);
        assert_eq!(targets(&cfg)?, [("full.tex", None), ("part1.tex", Some(1))]);
        let preamble = preamble(root, root, &cfg, "Paper title", "ja")?;
        assert!(preamble.starts_with(preamble_src));
        assert!(preamble.contains("\\title{Paper title}\n\\author{Author Name}\n"));
        assert_eq!(detect_engine(&preamble, "ja")?.args(), ["-lualatex"]);
        assert!(detect_engine("", "ja")?
            .args()
            .join(" ")
            .contains("upbibtex"));
        let bib = bibliography(root, root, &cfg, false)?;
        assert!(bib.contains(r"\bibliographystyle{amsplain}"));
        assert!(bib.contains(&format!(
            "\\bibliography{{{}/refs}}",
            root.canonicalize()?.display()
        )));
        let graph = Graph::open_local(root)?;
        let mut renderer = Renderer::new(&graph, "Cref", "ja");
        let refs = r"\Cref{thm:one,thm:two,card:third}";
        assert_eq!(renderer.link_refs(refs, true), refs);
        let tex = renderer.document(&preamble, &structure, None, "Proofs", &bib)?;
        let section = tex.find(r"\section{Opening}\label{sec:opening}").unwrap();
        let subsection = tex
            .find(r"\subsection{Results}\label{sec:results}")
            .unwrap();
        let statement = tex.find(r"\input{cards/first.statement.tex}").unwrap();
        let next = tex.find(r"\section{Next section}").unwrap();
        let deferred = tex
            .find(r"\input{cards/first.first.deferred-proof.tex}")
            .unwrap();
        let appendix = tex.find("\\appendix\n\\section{Proofs}").unwrap();
        assert!(
            section < subsection
                && subsection < statement
                && statement < next
                && next < deferred
                && deferred < appendix
        );
        let statement = &renderer.parts[Path::new("cards/first.statement.tex")].tex;
        assert_eq!(statement, "\\begin{theorem}[{A title [with brackets]}]\\label{thm:one}\nSee \\Cref{thm:two}, \\Cref{thm:two} and \\Cref{thm:two}. Existing \\Cref{thm:two}.\n\\end{theorem}\n");
        let proof = &renderer.parts[Path::new("cards/first.first.deferred-proof.tex")];
        assert_eq!(proof.tex, "\\begin{proof}[{\\Cref{thm:one}の証明}]\n\\emph{Step 1.} Begin with \\Cref{thm:two}.\n\n\\emph{Step 2.} Use Step 1, with $(1)$ unchanged.\n\\end{proof}\n");
        assert_eq!(proof.source.lines, [10, 10, 11, 11, 11]);
        assert!(renderer.parts[Path::new("cards/author.proof.tex")]
            .tex
            .contains(r"Author proof of \Cref{thm:one}."));
        assert!(renderer.parts[Path::new("cards/intro.statement.tex")]
            .tex
            .contains(r"\begin{introthm}"));
        let mut renderer = Renderer::new(&graph, "Cref", "ja");
        let part = renderer.document(&preamble, &structure, Some(1), "Proofs", &bib)?;
        assert!(!part.contains(r"\appendix") && !part.contains("Next section"));
        assert!(!renderer
            .parts
            .contains_key(Path::new("cards/first.first.deferred-proof.tex")));
        Ok(())
    }
}
