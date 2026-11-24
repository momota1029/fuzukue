mod feature;
mod figure;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::io::{Read, Seek};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use anyhow::{ensure, Context, Result};
use html5gum::{DefaultEmitter, Token, Tokenizer};
use regex::Regex;
use serde_json::{json, Value};

use crate::support::*;

const LEVELS: &[&str] = &["L1", "L2", "L3", "L4"];
const TEMPLATE: &str = include_str!("assets/companion-template.html");
const MARK: &str = "/*__COMPANION_DATA__*/null";

type Labels = BTreeMap<String, (String, u64)>;
type Methods<'a> = BTreeMap<&'a str, &'a Value>;
type Report<'a> = dyn FnMut(String) + 'a;

struct Spec<'a> {
    base: &'a Path,
    lang: &'a str,
    labels: Option<Labels>,
    methods: Methods<'a>,
    steps: BTreeSet<&'a str>,
}

fn default<'a>(value: &'a Value, key: &str, fallback: &'a str) -> &'a str {
    value[key].as_str().unwrap_or(fallback)
}

fn need(value: &Value, keys: &[&str], report: &mut Report) {
    for key in keys {
        if blank(&value[*key]) {
            report(format!("missing {key}"));
        }
    }
}

fn source(base: &Path, code: &Value) -> Result<(String, Vec<String>)> {
    let name = string(code, "file");
    ensure!(!name.is_empty(), "excerpt needs a file");
    let all = read(&base.join(name))?
        .split('\n')
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let first = code["first"]
        .as_u64()
        .context("excerpt needs a first line")?;
    let last = code["last"].as_u64().context("excerpt needs a last line")?;
    ensure!(
        first > 0 && first <= last && last <= all.len() as u64,
        "{name}: lines {first}–{last} outside the source"
    );
    let text = all[(first as usize - 1)..last as usize].join("\n");
    Ok((text, all))
}

fn fields_are_present(value: &Value, keys: &[&str]) -> bool {
    keys.iter().all(|key| !blank(&value[*key]))
}

fn numbered_fields(items: &Value, tag: &str, keys: &[&str], report: &mut Report) {
    for (index, item) in list(items).iter().enumerate() {
        need(item, keys, &mut |message| {
            report(format!("{tag} {}: {message}", index + 1))
        });
    }
}

fn line_range(value: &Value) -> Option<(u64, u64)> {
    let [first, last] = list(value) else {
        return None;
    };
    let (first, last) = (first.as_u64()?, last.as_u64()?);
    (first > 0 && first <= last).then_some((first, last))
}

fn in_range(range: &Value, code: &Value) -> bool {
    line_range(range).is_some_and(|(first, last)| {
        code["first"].as_u64().is_some_and(|start| start <= first)
            && code["last"].as_u64().is_some_and(|end| last <= end)
    })
}

fn na(value: &Value) -> bool {
    strval(value).starts_with("n/a:") && strval(value).chars().count() >= 6
}

fn read_labels(path: &Path) -> Result<Labels> {
    static LABEL: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?m)^\\newlabel\{([^}]+)\}\{\{([^}]*)\}\{(\d+)\}").unwrap());
    let text = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    Ok(LABEL
        .captures_iter(&String::from_utf8_lossy(&text))
        .filter_map(|row| {
            let (key, number, page) = (&row[1], &row[2], row[3].parse().ok()?);
            (!key.contains('@') && !number.is_empty()).then(|| (key.into(), (number.into(), page)))
        })
        .collect())
}

fn printed(key: &str, number: &str, lang: &str) -> String {
    if !number
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '.')
    {
        return number.into();
    }
    let kind = key.split(':').next().unwrap_or_default();
    match (kind, lang) {
        ("sec" | "def", _) => format!("§{number}"),
        ("eq", "ja") => format!("式({number})"),
        ("fig", "ja") => format!("図{number}"),
        ("df", "ja") => format!("定義{number}"),
        ("eq", _) => format!("Eq.({number})"),
        ("fig", _) => format!("Fig.{number}"),
        ("df", _) => format!("Def.{number}"),
        _ => number.into(),
    }
}

impl Spec<'_> {
    fn links(&self, keys: &Value, report: &mut Report) -> Value {
        let Some(labels) = &self.labels else {
            return json!([]);
        };
        Value::Array(list(keys).iter().filter_map(|key| {
            let key = strval(key);
            match labels.get(key) {
                Some((number, page)) => Some(json!({"key": key, "text": printed(key, number, self.lang), "page": page})),
                None => {
                    report(format!("note label '{key}' not in the note .aux"));
                    None
                }
            }
        }).collect())
    }

    fn crop(&self, figure: &Value, report: &mut Report) -> Value {
        if blank(&figure["paper_locator"]) {
            report("figure needs the paper figure locator".into());
        }
        let crop = string(figure, "paper_crop");
        if crop.is_empty() {
            if blank(&figure["paper_crop_absent"]) {
                report("no paper crop and no paper_crop_absent reason".into());
            }
            return with(figure, []);
        }
        let Ok(bytes) = fs::read(self.base.join(crop)) else {
            report(format!(
                "paper crop not found: {crop} (report it; do not replace it)"
            ));
            return with(figure, []);
        };
        let mimetype = match Path::new(crop)
            .extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str()
        {
            "jpeg" | "jpg" => "image/jpeg",
            "gif" => "image/gif",
            "svg" => "image/svg+xml",
            "webp" => "image/webp",
            _ => "image/png",
        };
        with(
            figure,
            [("paper_crop_data", json!(data_uri(mimetype, &bytes)))],
        )
    }

    fn lineage(&self, lineage: &Value, report: &mut Report) -> Value {
        if blank(lineage) {
            report("missing lineage (earlier methods before any code: what each was made for, its weakness, what the method keeps or changes)".into());
            return Value::Null;
        }
        numbered_fields(
            &lineage["families"],
            "family",
            &["family", "good", "weak"],
            report,
        );
        if blank(&lineage["entries"]) {
            report("no entries (one per earlier work the paper inherits from, compares with or criticises)".into());
        }
        let entries: Vec<_> = list(&lineage["entries"]).iter().map(|entry| {
            let name = default(entry, "name", "?");
            let mut entry_report = |message| report(format!("entry {name}: {message}"));
            need(entry, &["name", "made_for", "kept", "verified"], &mut entry_report);
            if blank(&entry["weakness"]) && !truth(&entry["reference"]) {
                entry_report("missing weakness (what the paper says it does badly); mark reference rows with reference: true".into());
            }
            with(entry, [("note_links", self.links(&entry["note_labels"], &mut entry_report))])
        }).collect();
        for (index, entry) in list(&lineage["after"]).iter().enumerate() {
            for key in ["name", "source", "point"] {
                if blank(&entry[key]) {
                    report(format!(
                        "later development {}: missing {key} (auxiliary, with its primary source)",
                        index + 1
                    ));
                }
            }
        }
        with(lineage, [("entries", json!(entries))])
    }

    fn code_map(&self, map: &Value, report: &mut Report) -> Value {
        if !map.is_object() {
            report(
                "missing code_map (the whole code of the entry function, coloured by step)".into(),
            );
            return Value::Null;
        }
        let entry = &map["entry"];
        let method = self
            .methods
            .get(default(entry, "impl", "author"))
            .copied()
            .unwrap_or(&Value::Null);
        let text = match source(&self.base.join(string(method, "root")), entry) {
            Ok((text, _)) => text,
            Err(err) => {
                report(format!("cannot read the entry function ({err})"));
                return Value::Null;
            }
        };
        let first = entry["first"].as_u64().unwrap();
        let last = entry["last"].as_u64().unwrap();
        let lines = text.split('\n').collect::<Vec<_>>();
        let gap = |from: u64, to: u64| {
            let mut nonempty = (from..to).filter(|n| {
                let line = lines[(*n - first) as usize].trim();
                !line.is_empty() && !line.starts_with('#')
            });
            nonempty
                .next()
                .map(|a| (a, nonempty.next_back().unwrap_or(a)))
        };
        let blocks = list(&map["blocks"]);
        let mut pos = first;
        for block in blocks {
            let (a, b) = (
                block["first"].as_u64().unwrap_or(0),
                block["last"].as_u64().unwrap_or(0),
            );
            if blank(&block["text"])
                || (!block["step"].is_null() && !self.steps.contains(strval(&block["step"])))
            {
                report(format!("block {a}-{b}: needs text and a step id of the overview (or null for bookkeeping)"));
            }
            if a < pos || b < a || b > last {
                report(format!(
                    "block {a}-{b} overlaps the previous block or leaves the entry function"
                ));
                continue;
            }
            if let Some((a, b)) = gap(pos, a) {
                report(format!("lines {a}-{b} are in no block (give them a block)"));
            }
            pos = b + 1;
        }
        if let Some((a, b)) = gap(pos, last + 1) {
            report(format!("lines {a}-{b} at the end are in no block"));
        }
        let extra = list(&map["extra"]);
        for step in &self.steps {
            if !blocks
                .iter()
                .chain(extra)
                .any(|block| &string(block, "step") == step)
            {
                report(format!(
                    "step '{step}' has no block (put the function that carries it in extra)"
                ));
            }
        }
        let extra: Vec<_> = extra
            .iter()
            .filter_map(|item| {
                let method = self
                    .methods
                    .get(default(item, "impl", "author"))
                    .copied()
                    .unwrap_or(&Value::Null);
                match source(&self.base.join(string(method, "root")), item) {
                    Ok((text, _)) => Some(with(item, [("text", json!(text))])),
                    Err(error) => {
                        report(format!(
                            "extra {}: cannot read ({error})",
                            string(item, "name")
                        ));
                        None
                    }
                }
            })
            .collect();
        let entry = with(
            entry,
            [
                ("text", json!(text)),
                ("impl_name", method["name"].clone()),
                ("commit", method["commit"].clone()),
                ("license", method["license"].clone()),
            ],
        );
        with(map, [("entry", entry), ("extra", json!(extra))])
    }
}

fn outline_problems(spec: &Value, problems: &mut Vec<String>) {
    let bg = &spec["background"];
    for key in ["picture", "summary", "question", "context", "terms"] {
        if blank(&bg[key]) {
            problems.push(format!("background: missing {key} (the shared introduction; picture = what is measured and what one wants to see; summary = the paper's Summary and Introduction in plain words)"));
        }
    }
    let primer = &bg["primer"];
    if blank(primer) {
        problems.push("background: missing primer (how to read what the method outputs and the figures that follow; 'n/a: reason' when the paper has none)".into());
    } else if !na(primer) {
        for key in ["object", "learn"] {
            if blank(&primer[key]) {
                problems.push(format!("background primer: missing {key} (what the output is, in plain words; what one can learn from it)"));
            }
        }
        for (key, fields, message) in [
            ("parts", ["term", "meaning"].as_slice(), "parts: the output taken apart (each term with its meaning)"),
            ("encodings", ["mark", "meaning", "figures"].as_slice(), "encodings: every mark, size or colour the later figures use, with its meaning and the figures (ids) that use it"),
        ] {
            if blank(&primer[key]) || list(&primer[key]).iter().any(|item| !fields_are_present(item, fields)) {
                problems.push(format!("background primer: {message}"));
            }
        }
        let figures: BTreeSet<_> = list(&spec["figures"])
            .iter()
            .map(|g| string(g, "id"))
            .collect();
        for encoding in list(&primer["encodings"]) {
            for id in list(&encoding["figures"]) {
                if !figures.contains(strval(id)) {
                    problems.push(format!("background primer: encoding '{}' cites figure '{}', which is not a figure id",
                        string(encoding, "mark"), strval(id)));
                }
            }
        }
    }

    let steps = list(&spec["overview"]);
    if steps.is_empty() {
        problems.push("overview: no whole-method map (list every step, built or not)".into());
    }
    let mut step_ids = BTreeSet::new();
    for step in steps {
        let id = default(step, "id", "?");
        need(
            step,
            &["id", "name", "role", "input", "output"],
            &mut |message| problems.push(format!("overview step {id}: {message}")),
        );
        if !step_ids.insert(id) {
            problems.push(format!("overview step {id}: duplicate id"));
        }
    }
    if !spec["difficulties"].is_null() {
        let difficulties = list(&spec["difficulties"]);
        let ids: BTreeSet<_> = difficulties.iter().map(|d| string(d, "id")).collect();
        if difficulties.is_empty() {
            problems.push(
                "difficulties: empty (list the obstacles the paper names, or omit the layer)"
                    .into(),
            );
        }
        if ids.len() != difficulties.len() {
            problems.push("difficulties: ids must be unique".into());
        }
        for d in difficulties {
            for key in ["id", "title", "text"] {
                if blank(&d[key]) {
                    problems.push(format!("difficulty {}: missing {key} (text = why the obvious route or the earlier method fails, not only what is hard)", default(d, "id", "?")));
                }
            }
        }
        let mut answered = BTreeSet::new();
        for step in steps {
            let id = default(step, "id", "?");
            if blank(&step["why"]) {
                problems.push(format!("overview step {id}: missing why (why this step is needed here; the storyline layer is on)"));
            }
            let answers = &step["answers"];
            if blank(answers) || (!na(answers) && !answers.is_array()) {
                problems.push(format!("overview step {id}: missing answers (ids of the difficulties it answers, or 'n/a: reason')"));
            }
            for answer in list(answers).iter().map(strval) {
                if !ids.contains(answer) {
                    problems.push(format!(
                        "overview step {id}: answers '{answer}', which is not a difficulty id"
                    ));
                }
                answered.insert(answer);
            }
        }
        for id in ids.difference(&answered) {
            problems.push(format!("difficulty {id}: no step answers it (an obstacle the method never meets, or a step is missing from the map)"));
        }
    }
}

fn load(path: &Path) -> Result<(Value, Vec<String>)> {
    let spec = crate::support::json(path)?;
    ensure!(spec.is_object(), "companion spec must be a JSON object");
    let base = path.parent().unwrap_or(Path::new("."));
    let mut problems = Vec::new();
    outline_problems(&spec, &mut problems);
    let labels = match spec["note"]["aux"].as_str().filter(|path| !path.is_empty()) {
        Some(aux) => match read_labels(&base.join(aux)) {
            Ok(labels) => Some(labels),
            Err(_) => {
                problems.push(format!("note: aux not found: {aux}"));
                None
            }
        },
        None => None,
    };
    let methods: Methods = std::iter::once(("author", &spec["methods"]["author"]))
        .chain(
            list(&spec["methods"]["prior"])
                .iter()
                .map(|method| (default(method, "id", "?"), method)),
        )
        .collect();
    let ctx = Spec {
        base,
        lang: default(&spec, "lang", "ja"),
        labels,
        methods,
        steps: list(&spec["overview"])
            .iter()
            .map(|step| string(step, "id"))
            .collect(),
    };
    let mut bg = with(&spec["background"], []);
    if truth(&bg["figure"]) {
        bg["figure"] = ctx.crop(&bg["figure"], &mut |msg| {
            problems.push(format!("background figure: {msg}"))
        });
    }
    for (id, method) in &ctx.methods {
        for key in ["name", "repo", "url", "commit", "license", "root"] {
            if blank(&method[key]) {
                problems.push(format!(
                    "method {id}: missing {key} (sources used: repository, URL, commit, licence)"
                ));
            }
        }
    }
    let code_map = ctx.code_map(&spec["code_map"], &mut |msg| {
        problems.push(format!("code_map: {msg}"))
    });
    let lineage = ctx.lineage(&spec["lineage"], &mut |msg| {
        problems.push(format!("lineage: {msg}"))
    });
    // The latest record replaces the value in the ID's first list position.
    let mut datasets = Vec::new();
    let mut dataset_positions = HashMap::new();
    for dataset in list(&spec["datasets"]) {
        let next = datasets.len();
        let position = *dataset_positions.entry(&dataset["id"]).or_insert(next);
        if position == next {
            datasets.push(dataset);
        } else {
            datasets[position] = dataset;
        }
    }
    if datasets.is_empty() {
        problems.push("datasets: no datasets listed (sources used)".into());
    }
    for &dataset in &datasets {
        let id = string(dataset, "id");
        need(
            dataset,
            &["name", "source", "identity", "level"],
            &mut |msg| problems.push(format!("dataset {id}: {msg}")),
        );
        match string(dataset, "level") {
            "AI" => for key in ["generator", "truth"] {
                if blank(&dataset[key]) {
                    problems.push(format!("dataset {id}: AI-created synthetic data needs {key} (script, and what the truth is)"));
                }
            },
            "L1" | "L2" | "L3" => (),
            _ => problems.push(format!("dataset {id}: level must be L1, L2 or L3 (data that was run), or 'AI' (synthetic demo data)")),
        }
    }
    for (i, x) in list(&spec["unobtained"]).iter().enumerate() {
        for key in ["what", "tried", "instead"] {
            if blank(&x[key]) {
                problems.push(format!(
                    "unobtained {}: missing {key} (what, places tried, what is used instead)",
                    i + 1
                ));
            }
        }
    }
    if spec.get("requests").is_none() {
        problems.push("requests: missing request list (use [] when nothing is open)".into());
    }
    numbered_fields(
        &spec["requests"],
        "request",
        &["what", "where", "serves", "enables"],
        &mut |msg| problems.push(msg),
    );
    let mut seen = BTreeSet::new();
    let features: Vec<_> = list(&spec["features"])
        .iter()
        .map(|item| {
            let id = default(item, "id", "?");
            if blank(&item["id"]) || !seen.insert(id) {
                problems.push(format!("step unit {id}: missing or duplicate id"));
            }
            feature::load(&ctx, item, &datasets, &mut |msg| {
                problems.push(format!("step unit {id}: {msg}"))
            })
        })
        .collect();
    let figures = figure::load(&ctx, &spec, &features, &mut problems);
    let methods = Value::Object(
        ctx.methods
            .iter()
            .map(|(&id, method)| {
                (
                    id.into(),
                    selected(
                        method,
                        &[
                            "name", "repo", "url", "commit", "license", "source", "files",
                        ],
                    ),
                )
            })
            .collect(),
    );
    let data = with(
        &selected(
            &spec,
            &[
                "title",
                "lang",
                "paper",
                "claims",
                "overview",
                "requests",
                "unobtained",
                "difficulties",
            ],
        ),
        [
            ("background", bg),
            ("datasets", json!(datasets)),
            ("methods", methods),
            ("lineage", lineage),
            ("code_map", code_map),
            ("features", json!(features)),
            ("figures", json!(figures)),
            (
                "note",
                json!({"pdf": default(&spec["note"], "pdf", ""), "deck_pdf": default(&spec["deck"], "pdf", "")}),
            ),
        ],
    );
    Ok((data, problems))
}

pub fn outline(spec: &Path) -> Result<bool> {
    let (_, problems) = load(spec)?;
    let (story, rest): (Vec<_>, Vec<_>) = problems.into_iter().partition(|p| {
        ["background", "overview", "difficult"]
            .iter()
            .any(|prefix| p.starts_with(prefix))
    });
    show(&story);
    println!(
        "{} problem(s) in the storyline layer ({} elsewhere, not checked yet)",
        story.len(),
        rest.len()
    );
    Ok(story.is_empty())
}

fn show(problems: &[String]) {
    if problems.is_empty() {
        println!("ok");
    } else {
        for problem in problems {
            println!("{problem}");
        }
    }
}

pub fn build(spec: &Path, out: &Path, template: Option<&Path>) -> Result<bool> {
    let (data, problems) = load(spec)?;
    if !problems.is_empty() {
        show(&problems);
        println!("{} problem(s); not built", problems.len());
        return Ok(false);
    }
    let template = template.map(read).transpose()?;
    let template = template.as_deref().unwrap_or(TEMPLATE);
    ensure!(template.contains(MARK), "template has no data marker");
    let payload = serde_json::to_string(&data)?.replace("</", "<\\/");
    let page = template.replace(MARK, &payload);
    write(out, &page)?;
    println!(
        "built {} ({} KB, {} feature(s))",
        out.display(),
        page.len() / 1024,
        list(&data["features"]).len()
    );
    Ok(true)
}

fn excerpts(data: &Value) -> BTreeMap<(&str, &str), (&str, Option<&str>)> {
    list(&data["features"])
        .iter()
        .flat_map(|f| {
            members(&f["codes"]).filter_map(move |(slot, code)| {
                code["sha256"].as_str().map(|sha| {
                    (
                        (string(f, "id"), slot.as_str()),
                        (sha, code["text"].as_str()),
                    )
                })
            })
        })
        .collect()
}

fn note_links(data: &Value) -> impl Iterator<Item = &Value> {
    list(&data["lineage"]["entries"])
        .iter()
        .chain(list(&data["features"]))
        .chain(list(&data["figures"]))
        .map(|entry| &entry["note_links"])
        .filter(|links| !list(links).is_empty())
}

pub fn check(
    spec: &Path,
    page: &Path,
    workspace: Option<&Path>,
    limit: u64,
    no_dom: bool,
    allow_pending: bool,
) -> Result<bool> {
    let (data, mut problems) = load(spec)?;
    let pending: Vec<_> = list(&data["figures"])
        .iter()
        .filter(|f| truth(&f["pending"]))
        .map(|f| string(f, "label"))
        .collect();
    if !allow_pending && !pending.is_empty() {
        problems.push(format!(
            "figures: {} figure unit(s) not built yet: {} (every main-text figure needs its unit)",
            pending.len(),
            pending.join(", ")
        ));
    }
    let html = read(page)?;
    let embedded = Regex::new(r"(?s)const DATA = (\{.*?\});\s*\n")?;
    if let Some(capture) = embedded.captures(&html) {
        match serde_json::from_str::<Value>(&capture[1]) {
            Ok(embedded) => {
                if string(&data["note"], "pdf") != string(&embedded["note"], "pdf")
                    || note_links(&data).ne(note_links(&embedded))
                {
                    problems.push(
                        "page: PDF links differ from the current note or AUX file; rebuild".into(),
                    );
                }
                let current = excerpts(&data);
                let embedded = excerpts(&embedded);
                for (feature, slot) in current
                    .keys()
                    .chain(embedded.keys())
                    .copied()
                    .collect::<BTreeSet<_>>()
                {
                    if current.get(&(feature, slot)) != embedded.get(&(feature, slot)) {
                        problems.push(format!("page: feature {feature} {slot} differs from the current source; rebuild"));
                    }
                }
            }
            Err(err) => problems.push(format!("page: malformed embedded data ({err})")),
        }
    } else {
        problems.push("page: no embedded data (was it built by build_companion?)".into());
    }
    const EXTERNAL: &[&str] = &[
        r#"<script[^>]+src\s*=\s*["']?(https?:)?//"#,
        r#"<link[^>]+href\s*=\s*["']?(https?:)?//"#,
        r#"<img[^>]+src\s*=\s*["']?(https?:)?//"#,
        r#"<iframe[^>]+src\s*=\s*["']?(https?:)?//"#,
        r#"url\(\s*["']?(https?:)?//"#,
        r"@import",
        r"\bfetch\s*\(",
        "XMLHttpRequest",
    ];
    let mut external = false;
    for pattern in EXTERNAL {
        if Regex::new(&format!("(?i){pattern}"))?.is_match(&html) {
            problems.push(format!(
                "page: external resource or network call ({pattern})"
            ));
            external = true;
        }
    }
    let kb = fs::metadata(page)?.len() / 1024;
    if kb > limit {
        problems.push(format!("page: {kb} KB over the {limit} KB limit"));
    }
    if let Some(root) = workspace {
        let status = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["check-ignore", "-q", "--"])
            .arg(page.canonicalize()?)
            .status()?;
        if !status.success() {
            problems.push(
                "page: output path is not ignored by the workspace Git (companions are private)"
                    .into(),
            );
        }
    }
    let note = if !no_dom && !external {
        match browser_dom(page) {
            Ok(dom) => {
                problems.extend(anchor_problems(&dom));
                None
            }
            Err(error) => Some(format!("rendered-page check skipped: {error}")),
        }
    } else {
        None
    };
    show(&problems);
    if let Some(note) = note {
        println!("{note}");
    }
    println!("{} problem(s)", problems.len());
    Ok(problems.is_empty())
}

pub(crate) fn browser_dom(path: &Path) -> Result<String> {
    let browser = std::env::var_os("CHROME")
        .map(PathBuf::from)
        .filter(|path| path.is_file())
        .or_else(|| {
            [
                "google-chrome",
                "chromium",
                "chromium-browser",
                "chrome",
                "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
            ]
            .into_iter()
            .find_map(executable)
        })
        .context("no Chrome/Chromium found (set CHROME)")?;
    let profile = tempfile::tempdir()?;
    let mut dom_file = tempfile::tempfile()?;
    let mut child = Command::new(browser)
        .args([
            "--headless=new",
            "--disable-gpu",
            "--disable-background-networking",
            "--no-first-run",
            "--no-default-browser-check",
            "--virtual-time-budget=30000",
            "--dump-dom",
        ])
        .arg(format!("--user-data-dir={}", profile.path().display()))
        .arg(file_url(path)?)
        .stdout(dom_file.try_clone()?)
        .stderr(Stdio::null())
        .spawn()
        .context("browser did not run")?;
    let deadline = Instant::now() + Duration::from_secs(120);
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("browser timed out");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    dom_file.rewind()?;
    let mut dom = String::new();
    dom_file.read_to_string(&mut dom)?;
    ensure!(
        status.success() && dom.contains("<main"),
        "browser produced no page"
    );
    Ok(dom)
}

pub(crate) fn anchor_problems(html: &str) -> Vec<String> {
    let mut counts = BTreeMap::new();
    let mut targets = BTreeSet::new();
    let mut emitter = DefaultEmitter::default();
    emitter.naively_switch_states(true);
    for token in Tokenizer::new_with_emitter(html.as_bytes(), emitter).flatten() {
        let Token::StartTag(tag) = token else {
            continue;
        };
        if let Some(id) = tag.attributes.get(b"id".as_slice()) {
            *counts
                .entry(String::from_utf8_lossy(id).into_owned())
                .or_insert(0) += 1;
        }
        if let Some(href) = tag.attributes.get(b"href".as_slice()) {
            let href = String::from_utf8_lossy(href);
            if let Some(target) = href.strip_prefix('#').filter(|s| !s.is_empty()) {
                targets.insert(target.to_owned());
            }
        }
    }
    let mut problems: Vec<_> = counts
        .iter()
        .filter(|(_, count)| **count > 1)
        .map(|(id, count)| format!("page: id \"{id}\" appears {count} times"))
        .collect();
    for target in targets {
        if !counts.contains_key(&target) {
            problems.push(format!("page: link #{target} points to no element"));
        }
    }
    problems
}
