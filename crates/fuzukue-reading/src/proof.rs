mod html;
mod review;
mod tex;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::LazyLock;

use anyhow::{ensure, Context, Result};
use fancy_regex::Regex as FancyRegex;
use regex::Regex;
use serde_json::{json, Value};

use crate::support::{self, *};

static STEP_REF: LazyLock<FancyRegex> =
    LazyLock::new(|| FancyRegex::new(r"(?<![\w.])([A-Z]\.\d+)(?![\w]|\.\d)").unwrap());
static LINE_REF: LazyLock<FancyRegex> = LazyLock::new(|| {
    FancyRegex::new(r"\(\s*([A-Z]\.\d+\.\d+)\s*\)|(?<![\w.])([A-Z]\.\d+\.\d+)(?![\w]|\.\d)")
        .unwrap()
});
static BLOCK_REF: LazyLock<FancyRegex> =
    LazyLock::new(|| FancyRegex::new(r"ブロック ([A-Z])(?:・段 (\d+))?(?![A-Za-z])").unwrap());

#[derive(Default)]
struct Block {
    id: String,
    title: String,
    claim: String,
    loc: String,
    intro: Vec<(String, bool)>,
    steps: Vec<Step>,
}

#[derive(Default)]
struct Step {
    id: String,
    title: String,
    claim: String,
    loc: String,
    ledger: String,
    items: Vec<Item>,
}

enum Item {
    Prose { text: String, narration: bool },
    Formula(Formula),
}

impl Item {
    fn text(&self) -> &str {
        match self {
            Self::Prose { text, .. } => text,
            Self::Formula(f) => &f.lead,
        }
    }

    fn formula(&self) -> Option<&Formula> {
        match self {
            Self::Formula(line) => Some(line),
            _ => None,
        }
    }
}

struct Formula {
    id: String,
    tex: String,
    lead: String,
    fine: Option<String>,
    reconstructed: bool,
}

struct Book {
    root: PathBuf,
    out: PathBuf,
    prefix: String,
    title: String,
    builder: String,
    context: Value,
    review: Value,
    ledger: BTreeMap<String, Value>,
    blocks: Vec<Block>,
    numbers: BTreeMap<String, String>,
    uses: BTreeMap<String, Vec<String>>,
    used_by: BTreeMap<String, Vec<String>>,
}

impl Book {
    fn stem(&self, suffix: &str) -> String {
        format!("{}{suffix}", self.prefix)
    }
    fn block_html(&self, id: &str) -> String {
        self.stem(&format!("-block-{id}.html"))
    }
    fn diff(&self, index: &Value) -> &Value {
        &self.context["diffs"][index.to_string().as_str()]
    }
    fn block_context(&self, id: &str) -> &Value {
        &self.context["block"][id]
    }
    fn step_context(&self, id: &str) -> &Value {
        &self.context["step"][id]
    }
    fn difficulty_ids(&self) -> Vec<u64> {
        let mut ids: Vec<_> = members(&self.context["diffs"])
            .filter_map(|(n, _)| n.parse().ok())
            .collect();
        ids.sort_unstable();
        ids
    }
    fn classification(&self, step: &Step) -> Option<String> {
        let entry = self.ledger.get(&step.ledger)?;
        let tier = match string(entry, "tier") {
            "key" => "鍵",
            "semi" => "準鍵",
            "standard" => "標準",
            _ => "?",
        };
        let origin = match string(entry, "origin") {
            value if value.starts_with("inherited") => "先行研究から引き継ぎ",
            "standard" => "標準",
            "adapted" => "既存の手法の適用・一般化",
            "new" => "原稿が新しいと主張",
            _ => "?",
        };
        Some(format!("{tier}・{origin}（暫定）"))
    }
}

fn parse(path: &Path) -> Result<Block> {
    let mut block = Block::default();
    let mut line_number = 0;
    for (index, raw) in read(path)?.lines().enumerate() {
        if raw.trim().is_empty() {
            continue;
        }
        if let Some(step) = raw.strip_prefix("step:") {
            let id = id(step.trim())?.to_owned();
            block.steps.push(Step {
                id,
                ..Step::default()
            });
            line_number = 0;
            continue;
        }
        let step = block.steps.last_mut();
        if let Some((text, narration)) = raw
            .strip_prefix(">> ")
            .map(|s| (s, true))
            .or_else(|| raw.strip_prefix("> ").map(|s| (s, false)))
        {
            let text = text.trim().to_owned();
            if let Some(step) = step {
                step.items.push(Item::Prose { text, narration });
            } else {
                block.intro.push((text, narration));
            }
            continue;
        }
        if let Some(source) = raw.strip_prefix("- ") {
            let mut parts = source.split(" @@ ").map(str::trim);
            let tex = parts.next().unwrap().to_owned();
            let lead = parts
                .next()
                .with_context(|| {
                    format!("{}:{}: line needs 'tex @@ lead'", path.display(), index + 1)
                })?
                .to_owned();
            let step = step.with_context(|| {
                format!("{}:{}: formula needs a step", path.display(), index + 1)
            })?;
            let mut fine = None;
            let mut reconstructed = false;
            for flag in parts {
                let (key, val) = flag.split_once(':').unwrap_or((flag, ""));
                match key.trim() {
                    "fine" => {
                        fine = Some(val.trim().to_owned());
                    }
                    "recon" => {
                        reconstructed = true;
                    }
                    _ => (),
                }
            }
            line_number += 1;
            step.items.push(Item::Formula(Formula {
                id: format!("{}.{line_number}", step.id),
                tex,
                lead,
                fine,
                reconstructed,
            }));
            continue;
        }
        let (key, value) = raw.split_once(':').unwrap_or((raw, ""));
        let value = value.trim().to_owned();
        if let Some(step) = step {
            match key.trim() {
                "title" => step.title = value,
                "claim" => step.claim = value,
                "loc" => step.loc = value,
                "ledger" => step.ledger = value,
                _ => (),
            }
        } else {
            match key.trim() {
                "block" => block.id = value,
                "title" => block.title = value,
                "claim" => block.claim = value,
                "loc" => block.loc = value,
                _ => (),
            }
        }
    }
    id(&block.id).with_context(|| format!("{}: block needs an id", path.display()))?;
    let mut ids = BTreeSet::new();
    for step in &block.steps {
        ensure!(
            ids.insert(&step.id),
            "{}: duplicate step {}",
            path.display(),
            step.id
        );
    }
    Ok(block)
}

fn texts(step: &Step) -> impl Iterator<Item = &str> {
    std::iter::once(step.claim.as_str()).chain(step.items.iter().map(Item::text))
}

fn outside_math(text: &str) -> String {
    text.split('$').step_by(2).collect()
}

fn line_refs(text: &str) -> Vec<String> {
    LINE_REF
        .captures_iter(text)
        .filter_map(Result::ok)
        .filter_map(|m| m.get(1).or_else(|| m.get(2)).map(|m| m.as_str().to_owned()))
        .collect()
}

fn numbering(block: &Block) -> Result<BTreeMap<String, String>> {
    let lines: BTreeMap<_, _> = block
        .steps
        .iter()
        .flat_map(|s| s.items.iter().filter_map(Item::formula))
        .map(|f| (&f.id, f))
        .collect();
    let texts = std::iter::once(block.claim.as_str())
        .chain(block.intro.iter().map(|(text, _)| text.as_str()))
        .chain(block.steps.iter().flat_map(texts));
    let refs: BTreeSet<_> = texts
        .flat_map(|text| line_refs(&outside_math(text)))
        .collect();
    for r in &refs {
        let formula = lines
            .get(r)
            .with_context(|| format!("reference to unknown line {r}"))?;
        ensure!(
            formula.fine.is_none(),
            "reference to hidden (fine) line {r}: the HTML does not show it"
        );
    }
    let mut numbers = BTreeMap::new();
    for step in &block.steps {
        for (i, line) in step
            .items
            .iter()
            .filter_map(Item::formula)
            .filter(|line| refs.contains(&line.id))
            .enumerate()
        {
            numbers.insert(line.id.clone(), format!("{}.{}", step.id, i + 1));
        }
    }
    Ok(numbers)
}

type References = BTreeMap<String, Vec<String>>;

fn usage(blocks: impl IntoIterator<Item = Block>) -> (References, References) {
    let mut uses = References::new();
    let mut used_by = References::new();
    for block in blocks {
        for step in block.steps {
            let mut seen = BTreeSet::new();
            for text in texts(&step) {
                let text = outside_math(text);
                let external = BLOCK_REF
                    .captures_iter(text.as_str())
                    .filter_map(Result::ok)
                    .filter_map(|m| Some(format!("{}.{}", &m[1], m.get(2)?.as_str())));
                let internal = STEP_REF
                    .captures_iter(text.as_str())
                    .filter_map(Result::ok)
                    .map(|m| m[1].to_owned())
                    .filter(|target| target.starts_with(&format!("{}.", block.id)));
                for target in external.chain(internal) {
                    if target != step.id && seen.insert(target.clone()) {
                        uses.entry(step.id.clone())
                            .or_default()
                            .push(target.clone());
                        used_by.entry(target).or_default().push(step.id.clone());
                    }
                }
            }
        }
    }
    (uses, used_by)
}

fn load(project: &Path, with_usage: bool) -> Result<Book> {
    let root = project.canonicalize()?;
    let config = support::json(&root.join("book.json"))?;
    let path = |key, default| root.join(config[key].as_str().unwrap_or(default));
    let optional = |key, default| -> Result<Value> {
        let path = path(key, default);
        if path.is_file() {
            support::json(&path)
        } else {
            Ok(Value::Null)
        }
    };
    let blocks_dir = path("blocks_dir", "blocks");
    let mut blocks = Vec::new();
    let mut numbers = BTreeMap::new();
    let mut ids = BTreeSet::new();
    for item in list(&config["order"]) {
        let bid = id(strval(item))?;
        ensure!(ids.insert(bid), "duplicate block {bid}");
        let block = parse(&blocks_dir.join(format!("{bid}.txt")))?;
        ensure!(
            block.id == bid,
            "block {} does not match requested {bid}",
            block.id
        );
        if with_usage {
            numbers.extend(numbering(&block)?);
        }
        blocks.push(block);
    }
    ensure!(
        !blocks.is_empty(),
        "book.json order needs at least one block"
    );
    let (uses, used_by) = if with_usage {
        let mut files = fs::read_dir(&blocks_dir)?.collect::<std::io::Result<Vec<_>>>()?;
        files.sort_by_key(|e| e.file_name());
        let all = files
            .into_iter()
            .filter(|file| file.path().extension().is_some_and(|ext| ext == "txt"))
            .map(|file| parse(&file.path()))
            .collect::<Result<Vec<_>>>()?;
        usage(all)
    } else {
        (References::new(), References::new())
    };
    let ledger = optional("ledger", "ledger/steps.json")?;
    let ledger = list(&ledger["steps"])
        .iter()
        .map(|step| (string(step, "id").to_owned(), step.clone()))
        .collect();
    let prefix = id(config["prefix"].as_str().unwrap_or("proof"))?.into();
    Ok(Book {
        out: path("out", "out"),
        prefix,
        title: config["title"].as_str().unwrap_or("証明").to_owned(),
        builder: config["builder"].as_str().unwrap_or("latexmk").to_owned(),
        context: optional("context", "context.json")?,
        review: optional("review", "review.json")?,
        root,
        ledger,
        blocks,
        numbers,
        uses,
        used_by,
    })
}

fn starts_with_relation(formula: &str) -> bool {
    let Some(command) = formula.strip_prefix('\\') else {
        return formula.starts_with(['=', '<', '>']);
    };
    // A TeX control word ends at the first non-letter.
    matches!(
        command.split(|c: char| !c.is_ascii_alphabetic()).next(),
        Some("le" | "leq" | "ge" | "geq")
    )
}

fn relations(formula: &str) -> Vec<usize> {
    let mut depth = 0i32;
    let mut cuts = Vec::new();
    for (offset, c) in formula.char_indices() {
        match c {
            '{' | '(' | '[' => depth += 1,
            '}' | ')' | ']' => depth -= 1,
            _ if depth == 0
                && !formula[..offset].ends_with('\\')
                && starts_with_relation(&formula[offset..]) =>
            {
                cuts.push(offset);
            }
            _ => (),
        }
    }
    cuts
}

fn continuation(formula: &str) -> bool {
    starts_with_relation(formula.trim_start())
}

#[derive(Default)]
struct Destination {
    number: Option<String>,
    page: u32,
}

type Aux = BTreeMap<String, Destination>;

fn parse_aux(text: &str) -> Result<Aux> {
    let label =
        Regex::new(r"\\newlabel\{([^}]+)\}\{\{((?:[^{}]|\{(?:[^{}]|\{[^{}]*\})*\})*)\}\{(\d+)\}")?;
    let mark = Regex::new(r"\\proofpage\{([^}]+)\}\{(\d+)\}")?;
    let mut aux = BTreeMap::new();
    for capture in label.captures_iter(text) {
        aux.insert(
            capture[1].to_owned(),
            Destination {
                number: Some(capture[2].replace(['{', '}'], "")),
                page: capture[3].parse()?,
            },
        );
    }
    for capture in mark.captures_iter(text) {
        aux.entry(capture[1].to_owned()).or_default().page = capture[2].parse()?;
    }
    Ok(aux)
}

fn render_url(book: &Book) -> Result<String> {
    let path = book.out.join(book.stem(".html"));
    let override_root = std::env::var_os("PROOF_BOOK_HTML_ROOT").map(PathBuf::from);
    let path = if let Some(root) = override_root {
        root.join(path.strip_prefix(&book.root)?)
    } else {
        path
    };
    Ok(file_url(&path)?
        .replace('~', "%7E")
        .replace('%', r"\ProofPercent "))
}

pub fn build(project: &Path, builder: Option<&str>) -> Result<bool> {
    let book = load(project, true)?;
    let src = book.out.join(".tex");
    fs::create_dir_all(&src)?;
    let _guard = fs::File::options()
        .create(true)
        .write(true)
        .truncate(false)
        .open(src.join(".build.lock"))?;
    _guard.lock()?;
    let url = render_url(&book)?;
    let mut split = BTreeSet::new();
    for pass in 0..5 {
        let main = tex::render(&book, &url, &split);
        write(&src.join("main.tex"), &main)?;
        tex::compile(&src, builder.unwrap_or(&book.builder))?;
        let log = read(&src.join(".temp/main.log"))?;
        let wide = tex::overfull(&main, &log)?;
        let new: Vec<_> = wide.difference(&split).cloned().collect();
        if new.is_empty() || pass == 4 {
            if !wide.is_empty() {
                println!("WARNING: still too wide after splitting: {wide:?}");
            }
            break;
        }
        println!("too wide -> one row per relation: {new:?}");
        split.extend(new);
    }
    let aux = parse_aux(&read(&src.join(".temp/main.aux"))?)?;
    for (id, number) in &book.numbers {
        let actual = aux.get(id).and_then(|d| d.number.as_deref());
        ensure!(
            actual == Some(number),
            "HTML and PDF equation numbers differ: {id} is {number} in HTML and {actual:?} in PDF"
        );
    }
    let pages: BTreeMap<_, _> = aux.iter().map(|(id, d)| (id, d.page)).collect();
    let outputs = html::render(&book, &aux)?;
    let pdf = fs::read(src.join(".temp/main.pdf"))?;
    ensure!(pdf.starts_with(b"%PDF-"), "TeX did not produce a PDF");
    write(&book.out.join(book.stem(".pdf")), pdf)?;
    write(
        &book.out.join("pages.json"),
        serde_json::to_vec_pretty(&pages)?,
    )?;
    for (file, html) in outputs {
        write(&book.out.join(file), html)?;
    }
    println!(
        "equations numbered: {} | ids mapped: {} | blocks: {} -> {}",
        book.numbers.len(),
        aux.len(),
        book.blocks.len(),
        book.out.display()
    );
    Ok(true)
}

pub fn refs(project: &Path, block: &str) -> Result<bool> {
    let block_id = id(block)?;
    let config = support::json(&project.join("book.json"))?;
    let directory = config["blocks_dir"].as_str().unwrap_or("blocks");
    let block = parse(&project.join(directory).join(format!("{block_id}.txt")))?;
    let lines: BTreeMap<_, _> = block
        .steps
        .iter()
        .flat_map(|s| s.items.iter().filter_map(Item::formula))
        .map(|f| (&f.id, f))
        .collect();
    for item in block.steps.iter().flat_map(|s| &s.items) {
        let refs = line_refs(&outside_math(item.text()));
        if refs.is_empty() {
            continue;
        }
        println!("{}:", item.formula().map_or("(prose)", |f| &f.id));
        for id in refs {
            let f = lines
                .get(&id)
                .with_context(|| format!("reference to unknown line {id}"))?;
            println!("    {id} = {}", f.tex.chars().take(70).collect::<String>());
        }
    }
    Ok(true)
}

pub fn check(project: &Path) -> Result<bool> {
    let book = load(project, false)?;
    let review = review::problems(&book);
    let mut count = review.len();
    println!(
        "review.json problems: {}",
        if review.is_empty() {
            "none".into()
        } else {
            review.join("; ")
        }
    );
    let log = read(&book.out.join(".tex/.temp/main.log"))?;
    let bad =
        Regex::new(r"Undefined|Multiply|undefined|Overfull \\hbox \((\d{2,})|Token not allowed")?;
    let bad: Vec<_> = log.lines().filter(|line| bad.is_match(line)).collect();
    count += bad.len();
    println!(
        "log problems: {}",
        if bad.is_empty() {
            "none".into()
        } else {
            bad.into_iter().take(6).collect::<Vec<_>>().join("\n")
        }
    );
    let html = book.out.join(book.stem(".html"));
    let source = read(&html)?;
    let (dom, rendered) = match crate::companion::browser_dom(&html) {
        Ok(dom) => (dom, true),
        Err(error) => {
            println!("KaTeX render check skipped: {error}");
            (source, false)
        }
    };
    let anchors = crate::companion::anchor_problems(&dom);
    for problem in &anchors {
        println!("{problem}");
    }
    count += anchors.len();
    if rendered {
        let errors = dom.matches("class=\"katex-error\"").count();
        println!(
            "katex errors: {errors} | dangling anchors: {}",
            anchors.len()
        );
        count += errors;
    }
    let pdf = book.out.join(book.stem(".pdf"));
    if executable("pdftotext").is_some() {
        let text = String::from_utf8(command(
            Command::new("pdftotext").arg("-bbox").arg(&pdf).arg("-"),
        )?)?;
        let pages = Regex::new(r#"(?s)<page width="([\d.]+)"[^>]*>(.*?)</page>"#)?;
        let spans = Regex::new(r#"\bxMax="([\d.]+)""#)?;
        let mut num_pages = 0;
        let mut overflow = 0;
        for page in pages.captures_iter(text.as_str()) {
            num_pages += 1;
            let margin = page[1].parse::<f64>()? - 40.0;
            overflow += spans
                .captures_iter(page.get(2).unwrap().as_str())
                .filter(|span| span[1].parse::<f64>().is_ok_and(|x| x > margin))
                .count();
        }
        println!("pages: {num_pages} | spans past right margin: {overflow}");
        count += overflow;
    } else {
        println!("pdftotext unavailable: PDF margin check skipped");
    }
    Ok(count == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proof_rendering_respects_relation_command_boundaries() -> Result<()> {
        let project = Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/fixtures/reading-companion/proof-relation-boundaries"
        ));
        let book = load(project, true)?;
        let tex = tex::render(&book, "book.html", &BTreeSet::new());
        let chains: Vec<_> = tex
            .split(r"\begin{align}")
            .skip(1)
            .map(|chain| chain.split_once(r"\end{align}").unwrap().0)
            .collect();
        assert_eq!(chains.len(), 2, "{chains:#?}");
        for row in [r"x&\ge2", r"&\le1", r"&\leq_{a}2"] {
            assert!(chains[0].contains(row), "{row} missing from {}", chains[0]);
        }
        assert!(chains[0].contains(r"\intertext{Digit after the command}"));
        assert!(chains[0].contains(r"\intertext{Underscore after the command}"));
        assert!(chains[1].contains(r"\left(y\right)&\geq3"));
        assert!(chains[1].contains(r"&\ge_42"));

        let pages = html::render(&book, &Aux::new())?;
        let html = &pages
            .iter()
            .find(|(name, _)| name == "boundaries.html")
            .unwrap()
            .1;
        for formula in [
            r"x&amp;\ge2",
            r"\phantom{x}&amp;\le1",
            r"\phantom{x}&amp;\leq_{a}2",
            r"\left(y\right)&amp;\geq3",
            r"\phantom{\left(y\right)}&amp;\ge_42",
        ] {
            assert!(
                html.contains(&format!(r"\begin{{aligned}}{formula}\end{{aligned}}")),
                "{formula} missing from the aligned HTML"
            );
        }
        Ok(())
    }
}
