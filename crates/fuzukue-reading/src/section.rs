mod code;
mod ledger;
mod original;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::LazyLock;

use anyhow::{ensure, Context, Result};
use regex::Regex;
use serde_json::Value;

use crate::support::*;

const KINDS: &[&str] = &["author", "official", "lib", "ai", "port", "none"];

struct Record {
    data: Value,
    base: PathBuf,
}

impl Record {
    fn sections(&self) -> &[Value] {
        list(&self.data["sections"])
    }
    fn figures(&self) -> &Value {
        &self.data["figures"]
    }
    fn section(&self, id: &str) -> Option<&Value> {
        self.sections().iter().find(|s| string(s, "id") == id)
    }
    fn children<'a>(&'a self, id: &'a str) -> impl Iterator<Item = &'a Value> {
        self.sections()
            .iter()
            .filter(move |s| string(s, "parent") == id)
    }
    fn ancestors(&self, id: &str) -> Vec<&str> {
        let mut result = Vec::new();
        let mut current = self.section(id);
        while let Some(parent) = current
            .map(|s| string(s, "parent"))
            .filter(|p| !p.is_empty())
        {
            if result.contains(&parent) || parent == id {
                break;
            }
            result.push(parent);
            current = self.section(parent);
        }
        result
    }

    fn edges<'a>(&'a self, id: &str, incoming: bool) -> Vec<(&'a str, &'a str)> {
        let ancestors = self.ancestors(id);
        let mut seen = BTreeSet::new();
        list(&self.data["edges"])
            .iter()
            .filter_map(|edge| {
                let [a, b, why] = list(edge) else { return None };
                let (a, b) = (strval(a), strval(b));
                if !seen.insert((a, b)) {
                    return None;
                }
                let other = if incoming && b == id {
                    a
                } else if !incoming && a == id {
                    b
                } else {
                    return None;
                };
                (!ancestors.contains(&other) && !self.ancestors(other).contains(&id))
                    .then_some((other, strval(why)))
            })
            .collect()
    }

    fn excerpt(&self, file: &str, first: u64, last: u64) -> Result<Vec<String>> {
        let text = read(&self.base.join(file))?;
        let lines: Vec<_> = text.split('\n').collect();
        ensure!(
            first >= 1 && first <= last && last <= lines.len() as u64,
            "{file}: lines {first}–{last} outside the source"
        );
        Ok(lines[first as usize - 1..last as usize]
            .iter()
            .map(|s| (*s).into())
            .collect())
    }
}

#[derive(Default)]
struct Diagnostics {
    errors: Vec<String>,
    warnings: Vec<String>,
}

impl Diagnostics {
    fn error(&mut self, text: impl Into<String>) {
        self.errors.push(text.into());
    }
    fn warn(&mut self, text: impl Into<String>) {
        self.warnings.push(text.into());
    }
}

fn diagnose(r: &Record) -> Diagnostics {
    static LOC: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\bpp?\.\s?\d+").unwrap());
    let mut d = Diagnostics::default();
    if !r.data["sections"].is_array() {
        d.error("sections must be an array");
    }
    let ids: BTreeSet<_> = r.sections().iter().map(|s| string(s, "id")).collect();
    if ids.len() != r.sections().len() {
        d.error("duplicate section ids");
    }
    for &section_id in &ids {
        if let Err(error) = id(section_id) {
            d.error(error.to_string());
        }
        let ancestors = r.ancestors(section_id);
        if let Some(last) = ancestors.last().copied() {
            if !ids.contains(last) {
                d.error(format!("{section_id}: unknown parent {last}"));
            } else if ancestors.iter().any(|parent| {
                r.section(parent).is_some_and(|s| {
                    string(s, "parent") == section_id
                        || (Some(parent) == ancestors.last()
                            && ancestors.contains(&string(s, "parent")))
                })
            }) {
                d.error(format!("{section_id}: cyclic parent relationship"));
            }
        }
        if r.section(section_id)
            .is_some_and(|s| string(s, "parent") == section_id)
        {
            d.error(format!("{section_id}: cyclic parent relationship"));
        }
    }
    for edge in list(&r.data["edges"]) {
        let [a, b, what] = list(edge) else {
            d.error("edge must be [section, section, label]");
            continue;
        };
        let (a, b, what) = (strval(a), strval(b), strval(what).trim());
        if !ids.contains(a) || !ids.contains(b) {
            d.error(format!("edge {a}->{b}: unknown section"));
        } else if r.ancestors(a).contains(&b) || r.ancestors(b).contains(&a) {
            d.warn(format!(
                "edge {a}->{b}: structure (ancestor/descendant), show as position"
            ));
        } else if what.chars().count() < 4
            || what == b
            || what == string(r.section(b).unwrap(), "title")
        {
            d.error(format!(
                "edge {a}->{b}: label must state what is passed, not the target's name"
            ));
        }
    }

    let concepts = list(&r.data["concepts"]);
    let cids: BTreeSet<_> = concepts.iter().map(|c| string(c, "id")).collect();
    let homed: BTreeSet<_> = r
        .sections()
        .iter()
        .flat_map(|s| list(&s["concepts"]).iter().map(strval))
        .collect();
    for c in concepts {
        let id = string(c, "id");
        for key in ["gist", "explain", "definition"] {
            if blank(&c[key]) {
                d.error(format!("concept {id}: '{key}' is required (gist: one or two lines; explain: role, intuition, where it appears; definition: precise)"));
            }
        }
        if !blank(&c["explain"]) && string(c, "explain").trim() == string(c, "definition").trim() {
            d.warn(format!("concept {id}: explain repeats the definition; say what it is for and where it appears"));
        }
        if string(c, "gist").chars().count() > 200 {
            d.warn(format!("concept {id}: gist is longer than two lines"));
        }
        if !homed.contains(id) {
            d.error(format!("concept {id}: not listed in any section's 'concepts' (the two ledgers link through the home section)"));
        }
    }
    let mut used = BTreeSet::new();
    for s in r.sections() {
        let i = string(s, "id");
        if !LOC.is_match(string(s, "evidence")) {
            d.error(format!("{i}: evidence lacks a page locator (p. N)"));
        }
        for key in ["lead", "bridge"] {
            if blank(&s[key])
                && (key == "lead" || list(&r.data["edges"]).iter().any(|e| strval(&e[0]) == i))
            {
                d.warn(format!(
                    "{i}: no '{key}' paragraph (entrance and bridge carry the connections)"
                ));
            }
        }
        if blank(&s["glossary"])
            && (s["weights"][0].as_u64().unwrap_or(1) + s["weights"][1].as_u64().unwrap_or(1) > 0)
        {
            d.warn(format!(
                "{i}: no glossary (terms and what they are good for)"
            ));
        }
        for concept in list(&s["concepts"]).iter().map(strval) {
            if !cids.contains(concept) {
                d.error(format!(
                    "{i}: concept {concept} is not defined in the record's concepts"
                ));
            }
        }
        for claim in list(&s["claims"]) {
            for concept in list(&claim["concepts"]).iter().map(strval) {
                if !cids.contains(concept) {
                    d.error(format!(
                        "{i}: claim {} refers to unknown concept {concept}",
                        string(claim, "id")
                    ));
                }
            }
        }
        for term in list(&s["glossary"]) {
            let word = strval(&term[0]).trim();
            if strval(&term[1]).trim().chars().count() < 3 * word.chars().count() {
                d.warn(format!("{i}: glossary term '{word}' is explained in fewer words than a name needs; give one or two lines (what it is, what it is good for)"));
            }
        }
        if blank(&s["answer"]) || blank(&s["question"]) {
            d.error(format!("{i}: question and answer are required"));
        }
        if range(&s["pages"]).is_none() {
            d.error(format!("{i}: bad pages"));
        }
        for f in list(&s["figures"]).iter().map(strval) {
            used.insert(f);
            if r.figures().get(f).is_none() {
                d.error(format!("{i}: unknown figure {f}"));
            }
        }
        code::diagnose(s, &mut d);
    }
    for (f, figure) in members(r.figures()) {
        if !used.contains(f.as_str()) {
            d.error(format!("figure {f} is not used by any section"));
        }
        if !truth(&figure["color_checked"]) {
            d.error(format!(
                "figure {f}: colour legend not checked against the image and caption"
            ));
        }
        if blank(&figure["text"]) {
            d.warn(format!(
                "figure {f}: no reading text (what to look at, panel by panel)"
            ));
        }
        let terms = list(&figure["terms"]);
        if terms.is_empty() {
            d.warn(format!("figure {f}: no terms for reading it (each as [term, what it is and is good for, basis])"));
        }
        if terms
            .iter()
            .any(|t| list(t).len() != 3 || list(t).iter().any(blank))
        {
            d.error(format!("figure {f}: a term must be [term, explanation, basis] (basis: the caption or text, or 'AI lookup, unchecked')"));
        }
    }
    d
}

fn range(value: &Value) -> Option<(u64, u64)> {
    let [a, b] = list(value) else { return None };
    let (a, b) = (a.as_u64()?, b.as_u64()?);
    (a >= 1 && a <= b).then_some((a, b))
}

fn pages(section: &Value) -> String {
    let (first, last) = range(&section["pages"]).unwrap_or_default();
    if first == last {
        format!("p.{first}")
    } else {
        format!("pp.{first}–{last}")
    }
}

fn kind(key: &str) -> &str {
    match key {
        "lib" => "library",
        "ai" => "AI stand-in",
        "port" => "AI port",
        _ => key,
    }
}

fn badge(key: &str) -> String {
    format!(
        r#"<span class="by by-{}">{}</span>"#,
        escape(key),
        escape(kind(key))
    )
}

fn active_code(section: &Value) -> impl Iterator<Item = &Value> {
    list(&section["code"])
        .iter()
        .filter(|c| string(c, "kind") != "none")
}

fn html(input: &str) -> String {
    if !input.contains('$') || executable("pandoc").is_none() {
        return escape(input);
    }
    let rendered = (|| -> Result<String> {
        let mut child = Command::new("pandoc")
            .args(["-f", "latex", "-t", "html", "--mathml"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        child
            .stdin
            .take()
            .context("pandoc stdin unavailable")?
            .write_all(input.as_bytes())?;
        let output = child.wait_with_output()?;
        ensure!(output.status.success(), "pandoc failed");
        let text = String::from_utf8(output.stdout)?;
        let text = text.trim();
        ensure!(!text.is_empty(), "pandoc produced no HTML");
        Ok(text
            .strip_prefix("<p>")
            .and_then(|s| s.strip_suffix("</p>"))
            .unwrap_or(text)
            .into())
    })();
    rendered.unwrap_or_else(|_| escape(input))
}

fn mh(value: &Value, key: &str) -> String {
    html(string(value, key))
}
fn eh(value: &Value, key: &str) -> String {
    escape(string(value, key))
}

fn tex(input: &str) -> String {
    static MATH: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\$[^$]*\$").unwrap());
    let mut output = String::new();
    let mut cursor = 0;
    let plain = |text: &str, output: &mut String| {
        for c in text.chars() {
            match c {
                '\\' => output.push_str(r"\textbackslash{}"),
                '_' | '#' | '&' | '%' | '{' | '}' => {
                    output.push('\\');
                    output.push(c);
                }
                _ => output.push(c),
            }
        }
    };
    for m in MATH.find_iter(input) {
        plain(&input[cursor..m.start()], &mut output);
        output.push_str(m.as_str());
        cursor = m.end();
    }
    plain(&input[cursor..], &mut output);
    output
}

fn tx(value: &Value, key: &str) -> String {
    tex(string(value, key))
}

fn section_link(id: &str) -> String {
    format!(r##"<a href="#sec-{0}">§{0}</a>"##, escape(id))
}

fn fold(id: &str, class: &str, title: &str, body: &str, open: bool) -> String {
    let open = if open { " open" } else { "" };
    format!(
        r#"<details class="{class}" id="{id}"{open}><summary>{title}</summary><div class="inner">{body}</div></details>"#
    )
}

fn fallback(value: String, default: &str) -> String {
    if value.is_empty() {
        default.into()
    } else {
        value
    }
}

fn panel_html(r: &Record, s: &Value) -> String {
    let id = string(s, "id");
    let mut position = r
        .ancestors(id)
        .into_iter()
        .rev()
        .map(section_link)
        .collect::<Vec<_>>();
    position.push(format!("§{}", escape(id)));
    let mut position = position.join(" ＞ ");
    let children = r
        .children(id)
        .map(|k| section_link(string(k, "id")))
        .collect::<Vec<_>>();
    if !children.is_empty() {
        write!(position, "  sections: {}", children.join(", ")).unwrap();
    }
    let mut rows = vec![
        ("Position", position),
        ("Question", mh(s, "question")),
        ("Answer", mh(s, "answer")),
        ("Evidence", mh(s, "evidence")),
        ("Selling point", mh(s, "sell")),
    ];
    for (label, incoming) in [("Receives", true), ("Hands on", false)] {
        let links = r
            .edges(id, incoming)
            .into_iter()
            .map(|(other, why)| format!("{} {}", section_link(other), html(why)))
            .collect::<Vec<_>>()
            .join("<br>");
        rows.push((label, fallback(links, "—")));
    }
    if !blank(&s["figures"]) {
        let figures = list(&s["figures"]).iter().map(|f| {
            let f = strval(f);
            let data = &r.figures()[f];
            format!(r##"<a href="#fig-{f}">{f}</a> (p.{}): {} <span class="muted">colour: {}</span>"##,
                data["page"], mh(data, "what"), mh(data, "color"), f=escape(f))
        }).collect::<Vec<_>>().join("<br>");
        rows.push(("Figures", figures));
    }
    let code = active_code(s)
        .map(|c| {
            format!(
                r#"{} <b>{}</b> {} <span class="muted">[{}]</span>"#,
                badge(string(c, "kind")),
                eh(c, "name"),
                eh(c, "src"),
                eh(c, "state")
            )
        })
        .collect::<Vec<_>>()
        .join("<br>");
    rows.push(("Code", fallback(code, "no code in this section")));
    let materials = list(&s["materials"])
        .iter()
        .map(|mat| {
            let state = string(mat, "state");
            let label = match state {
                "alt" => "alternative only",
                "req" => "requested",
                "na" => "n/a",
                _ => state,
            };
            format!(
                r#"{}: <span class="mat {}">{}</span> <span class="muted">{}</span>"#,
                eh(mat, "item"),
                escape(state),
                escape(label),
                eh(mat, "note")
            )
        })
        .collect::<Vec<_>>()
        .join("<br>");
    rows.push(("Materials", fallback(materials, "—")));
    format!(
        r#"<div class="panel2"><table>{}</table></div>"#,
        rows.into_iter()
            .map(|(k, v)| format!("<tr><th>{k}</th><td>{v}</td></tr>"))
            .collect::<String>()
    )
}

fn panel_tex(r: &Record, s: &Value) -> String {
    let id = string(s, "id");
    let mut rows = vec![
        ("Question", tx(s, "question")),
        ("Answer", tx(s, "answer")),
        ("Evidence", tx(s, "evidence")),
        ("Selling point", tx(s, "sell")),
    ];
    for (label, incoming) in [("Receives", true), ("Hands on", false)] {
        let edges = r
            .edges(id, incoming)
            .into_iter()
            .map(|(other, why)| format!(r"\S{other} {}", tex(why)))
            .collect::<Vec<_>>()
            .join("; ");
        rows.push((label, fallback(edges, "--")));
    }
    for f in list(&s["figures"]).iter().map(strval) {
        let fig = &r.figures()[f];
        rows.push((
            "Figure",
            format!(
                "{} (p.{}): {} / colour: {}",
                tex(f),
                fig["page"],
                tx(fig, "what"),
                tx(fig, "color")
            ),
        ));
    }
    let code = active_code(s)
        .map(|c| format!("[{}] {}", kind(string(c, "kind")), tx(c, "name")))
        .collect::<Vec<_>>()
        .join("; ");
    rows.push(("Code", fallback(code, "none")));
    let mut result = String::new();
    if !blank(&s["lead"]) {
        write!(
            result,
            r"\par\medskip\noindent{{\small\textbf{{Entrance.}} {}}}\par",
            tx(s, "lead")
        )
        .unwrap();
    }
    write!(result, r"\par\medskip\noindent\begin{{tcolorbox}}[colback=black!2,boxrule=0.4pt,fontupper=\small]{{\small\bfseries Section structure: \S{id} {} ({})}}\par\smallskip\begin{{tabular}}{{@{{}}>{{\raggedright\bfseries}}p{{2.4cm}}@{{\hspace{{4pt}}}}>{{\raggedright\arraybackslash}}p{{12.6cm}}@{{}}}}", tx(s, "title"), pages(s)).unwrap();
    for (key, value) in rows {
        writeln!(result, r"{key} & {value} \\[2pt]").unwrap();
    }
    result.push_str(r"\end{tabular}\end{tcolorbox}\par\medskip");
    result.push('\n');
    result
}

pub fn check(path: &Path) -> Result<bool> {
    let r = Record {
        data: json(path)?,
        base: path.parent().unwrap_or(Path::new(".")).into(),
    };
    let diags = diagnose(&r);
    for warning in &diags.warnings {
        println!("warning: {warning}");
    }
    for error in &diags.errors {
        println!("ERROR: {error}");
    }
    println!(
        "{} errors, {} warnings",
        diags.errors.len(),
        diags.warnings.len()
    );
    Ok(diags.errors.is_empty())
}

fn glossary(s: &Value, full: bool) -> String {
    let rows = list(&s["glossary"]);
    if rows.is_empty() {
        return String::new();
    }
    let mut out = if full {
        "<table><tr><th>term</th><th>plain explanation and what it is good for</th><th>where</th><th>field</th></tr>".to_owned()
    } else {
        "<table><tr><th>term</th><th>what it is and what it is good for</th></tr>".to_owned()
    };
    for row in rows {
        write!(
            out,
            "<tr><td><b>{}</b></td><td>{}</td>",
            html(strval(&row[0])),
            html(strval(&row[1]))
        )
        .unwrap();
        if full {
            write!(
                out,
                r#"<td class="small">{}</td><td class="small">{}</td>"#,
                escape(strval(&row[2])),
                escape(strval(&row[3]))
            )
            .unwrap();
        }
        out.push_str("</tr>");
    }
    out.push_str("</table>");
    out
}

fn link_to(target: &str, text: &str) -> String {
    format!(r##"<a href="#{}">{}</a>"##, escape(target), escape(text))
}

fn figure_card(r: &Record, sid: &str, figure: &str, flow: bool) -> String {
    let f = &r.figures()[figure];
    let image = if blank(&f["image"]) {
        String::new()
    } else {
        format!(r#"<img src="{}" alt="{}">"#, eh(f, "image"), escape(figure))
    };
    let links = list(&r.data["figure_links"]);
    let name = escape(figure);
    let title = format!("<h4>{name} (p.{})</h4>", f["page"]);
    let explanation = format!(
        "<p>{}</p><p>{}</p><p><b>Colour:</b> {}</p>",
        mh(f, "what"),
        mh(f, "text"),
        mh(f, "color")
    );
    if !flow {
        let related: String = links
            .iter()
            .filter(|link| strval(&link[0]) == figure || strval(&link[1]) == figure)
            .map(|l| {
                format!(
                    "<li>{} → {}: <b>{}</b>. {}</li>",
                    escape(strval(&l[0])),
                    escape(strval(&l[1])),
                    escape(strval(&l[2])),
                    html(strval(&l[3]))
                )
            })
            .collect();
        let related = if related.is_empty() {
            related
        } else {
            format!("<ul>{related}</ul>")
        };
        return format!(
            r#"<div class="figin"><div>{image}</div><div>{title}{explanation}{related}</div></div>"#
        );
    }
    let prev: String = links
        .iter()
        .filter(|l| strval(&l[1]) == figure)
        .map(|l| {
            format!(
                r#"<div class="fromprev"><b>From {}</b>: {}. {}</div>"#,
                html(strval(&l[0])),
                escape(strval(&l[2])),
                html(strval(&l[3]))
            )
        })
        .collect();
    let terms: String = list(&f["terms"])
        .iter()
        .map(|t| {
            format!(
                r#"<tr><td><b>{}</b></td><td>{}</td><td class="small">{}</td></tr>"#,
                html(strval(&t[0])),
                html(strval(&t[1])),
                escape(strval(&t[2]))
            )
        })
        .collect();
    let terms = if terms.is_empty() {
        r#"<p class="muted small">(no terms recorded)</p>"#.into()
    } else {
        format!("<table><tr><th>term</th><th>what it is and what it is good for</th><th>basis</th></tr>{terms}</table>")
    };
    let other = r
        .sections()
        .iter()
        .filter(|s| {
            string(s, "id") != sid && list(&s["figures"]).iter().any(|f| strval(f) == figure)
        })
        .map(|s| section_link(string(s, "id")))
        .collect::<Vec<_>>()
        .join(", ");
    let occurrences = if other.is_empty() {
        "This figure occurs only in this section.".into()
    } else {
        format!("Also in: {other}.")
    };
    format!(
        r#"<div class="figcard" id="ff-{sid}-{name}">{title}{prev}<div class="figin"><div>{image}</div><div>{explanation}</div></div><div class="figterms"><b>Terms for reading this figure</b>{terms}</div><p class="small">{occurrences}</p></div>"#
    )
}

fn says(s: &Value) -> String {
    format!(
        r#"<div class="say"><b>What this section says</b>: {} → {}</div>"#,
        mh(s, "question"),
        mh(s, "answer")
    )
}

fn code_views(r: &Record, s: &Value, flow: bool) -> Result<String> {
    let plain = code::steps_html(r, s, false)?;
    let annotated = code::steps_html(r, s, true)?;
    let body = if flow {
        let legend = if blank(&s["comparison"]) {
            r#"<p class="small muted">(no comparison)</p>"#.into()
        } else {
            format!(
                r#"<p class="small"><span class="stl same">same</span> <span class="stl chg">changed</span> <span class="stl onl">only on one side</span> {}. {}</p>"#,
                eh(&s["comparison"], "title"),
                escape(&code::summary(&s["comparison"]))
            )
        };
        format!(
            r#"<div class="flow goal"><b>Goal, and why this tool</b><p>{}</p><p>{}</p></div><details class="plaincode" open><summary><b>The code as a whole, with nothing added (a heading per Step only)</b></summary>{plain}</details><details class="annotated"><summary><b>The same code with a note above every line (comparison counterpart, called-function specs and math correspondence included)</b></summary>{legend}{annotated}</details>"#,
            mh(s, "flow"),
            mh(s, "why")
        )
    } else {
        let mut out = "<p>Steps below show verbatim lines; the math correspondence follows each code block.</p>".to_owned();
        if !blank(&s["flow"]) {
            let why = if blank(&s["why"]) {
                String::new()
            } else {
                format!("<p>{}</p>", mh(s, "why"))
            };
            write!(out, r#"<details class="flow goal" open><summary><b>Goal of this code as a whole, and why this tool</b></summary><p>{}</p>{why}</details>"#, mh(s, "flow")).unwrap();
        }
        write!(out, r#"<details class="plaincode" open><summary><b>The code as a whole, with nothing added</b></summary>{plain}</details><details class="annotated"><summary><b>The same code with a note above every line (comparison counterpart and called-function specs on the line)</b></summary>{annotated}</details>"#).unwrap();
        if !blank(&s["after"]) {
            write!(out, r#"<div class="flow after"><b>Meaning of the result, and where it leads</b><p>{}</p></div>"#,
                mh(s, "after")).unwrap();
        }
        out.push_str(&code::comparison_html(r, s)?);
        out
    };
    Ok(body)
}

type Regions = BTreeMap<String, original::Region>;

fn section_html(r: &Record, s: &Value, regions: &Regions) -> Result<String> {
    let id = string(s, "id");
    let mut body = says(s);
    body.push_str(&fold(
        &format!("p-{id}"),
        "blk",
        "Section structure",
        &panel_html(r, s),
        true,
    ));
    body.push_str(&fold(
        &format!("ot-{id}"),
        "blk",
        &format!("Original and faithful translation ({})", pages(s)),
        &regions[id].render(None),
        false,
    ));
    if !blank(&s["figures"]) {
        let figs: String = list(&s["figures"])
            .iter()
            .map(|f| figure_card(r, id, strval(f), false))
            .collect();
        body.push_str(&fold(
            &format!("figs-{id}"),
            "blk",
            "Figures of this section",
            &figs,
            false,
        ));
    }
    if !blank(&s["steps"]) {
        body.push_str(&fold(
            &format!("cu-{id}"),
            "blk",
            "Code: whole code, annotated version, comparison",
            &code_views(r, s, false)?,
            false,
        ));
    }
    let terms = glossary(s, true);
    if !terms.is_empty() {
        body.push_str(&fold(
            &format!("gl-{id}"),
            "blk",
            &format!("Terms in this section ({})", list(&s["glossary"]).len()),
            &terms,
            false,
        ));
    }
    let ledger = s["ledger_html"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| ledger::section_html(r, s));
    if !ledger.is_empty() {
        body.push_str(&fold(
            &format!("led-{id}"),
            "blk",
            "Math ledger of this section",
            &ledger,
            false,
        ));
    }
    if let Some(file) = s["explanation_html"].as_str().filter(|s| !s.is_empty()) {
        body.push_str(&fold(
            &format!("ex-{id}"),
            "blk",
            "Explanation",
            &read(&r.base.join(file))?,
            false,
        ));
    }
    for child in r.children(id) {
        body.push_str(&section_html(r, child, regions)?);
    }
    if !blank(&s["bridge"]) {
        write!(
            body,
            r#"<div class="bridge"><b>To the next section</b><p>{}</p></div>"#,
            mh(s, "bridge")
        )
        .unwrap();
    }
    let weights = ["math", "code", "fig"]
        .into_iter()
        .enumerate()
        .map(|(i, name)| format!("{name}{}", s["weights"][i].as_i64().unwrap_or(0)))
        .collect::<Vec<_>>()
        .join(" ");
    let kinds = active_code(s)
        .map(|c| badge(string(c, "kind")))
        .collect::<Vec<_>>()
        .join(" ");
    let title = format!(
        r#"§{id} {} <span class="muted small">{} {weights}</span> {kinds}<div class="small" style="font-weight:400">{}</div>"#,
        eh(s, "title"),
        pages(s),
        mh(s, "sell")
    );
    Ok(fold(&format!("sec-{id}"), "blk", &title, &body, false))
}

fn heading(s: &Value) -> String {
    format!(
        r#"§{} {} <span class="muted small">{}</span>"#,
        eh(s, "id"),
        eh(s, "title"),
        pages(s)
    )
}

fn nested_heading(s: &Value) -> (usize, String) {
    let level = string(s, "id").chars().filter(|&c| c == '.').count();
    let tag = 3 + level.min(2);
    (level, format!("<h{tag}>{}</h{tag}>", heading(s)))
}

fn reading_html(r: &Record, originals: &Regions) -> String {
    let mut chapters = Vec::<(&Value, String)>::new();
    for s in r.sections() {
        let id = string(s, "id");
        let (level, head) = nested_heading(s);
        if blank(&s["parent"]) || chapters.is_empty() {
            chapters.push((s, String::new()));
        }
        let out = &mut chapters.last_mut().unwrap().1;
        let region = &originals[id];
        write!(out, r#"<div class="rd lv{level}" id="rd-{id}">{head}"#).unwrap();
        if region.translation.is_empty() {
            out.push_str(r#"<p class="muted small">(This section is only a heading; its text starts in the subsections below.)</p></div>"#);
            continue;
        }
        out.push_str(&region.render(Some(&format!("ot-{id}"))));
        out.push_str(&says(s));
        let mut links = vec![link_to(&format!("sec-{id}"), "deep dive")];
        if !blank(&s["figures"]) {
            let figures = list(&s["figures"])
                .iter()
                .map(|f| link_to(&format!("figs-{id}"), strval(f)))
                .collect::<Vec<_>>()
                .join(", ");
            links.push(format!("figures: {figures}"));
        }
        if !blank(&s["steps"]) {
            links.push(link_to(&format!("cu-{id}"), "code"));
        }
        write!(out, r#"<p class="small">{}</p></div>"#, links.join(" / ")).unwrap();
    }
    let mut out = String::new();
    for (n, (top, body)) in chapters.iter().enumerate() {
        out.push_str(&fold(
            &format!("rdc-{}", string(top, "id")),
            "blk rdch",
            &heading(top),
            body,
            n == 0,
        ));
    }
    out.push_str(
        r#"<div class="rd" id="whole"><h3>The whole picture (read after all sections)</h3>"#,
    );
    if !blank(&r.data["whole_picture"]) {
        write!(out, "<p>{}</p>", mh(&r.data, "whole_picture")).unwrap();
    }
    out.push_str("<ol>");
    for (top, _) in chapters {
        let mut last = top;
        while let Some(child) = r.children(string(last, "id")).last() {
            last = child;
        }
        write!(
            out,
            "<li>§{} <b>{}</b>: {} → {}",
            eh(top, "id"),
            eh(top, "title"),
            mh(top, "question"),
            mh(top, "answer")
        )
        .unwrap();
        if !blank(&last["bridge"]) {
            write!(
                out,
                r#"<br><span class="small">→ next chapter: {}</span>"#,
                mh(last, "bridge")
            )
            .unwrap();
        }
        out.push_str("</li>");
    }
    out.push_str("</ol></div>");
    out
}

fn chapter_folds(r: &Record, items: Vec<(&str, String, Vec<&str>)>, prefix: &str) -> String {
    let mut by = BTreeMap::<_, Vec<_>>::new();
    for (id, html, names) in items {
        by.entry(r.ancestors(id).last().copied().unwrap_or(id))
            .or_default()
            .push((html, names));
    }
    r.sections()
        .iter()
        .filter_map(|s| by.remove(string(s, "id")).map(|group| (s, group)))
        .enumerate()
        .map(|(n, (s, group))| {
            let mut seen = BTreeSet::new();
            let names = group
                .iter()
                .flat_map(|(_, names)| names)
                .filter(|name| seen.insert(**name))
                .copied()
                .collect::<Vec<_>>()
                .join(", ");
            let body: String = group.into_iter().map(|(html, _)| html).collect();
            fold(
                &format!("{prefix}-{}", string(s, "id")),
                "blk",
                &format!("{} — {}", heading(s), escape(&names)),
                &body,
                n == 0,
            )
        })
        .collect()
}

fn figures_html(r: &Record) -> String {
    let items = r
        .sections()
        .iter()
        .filter(|s| !blank(&s["figures"]))
        .map(|s| {
            let id = string(s, "id");
            let (_, head) = nested_heading(s);
            let names: Vec<_> = list(&s["figures"]).iter().map(strval).collect();
            let figs: String = names.iter().map(|f| figure_card(r, id, f, true)).collect();
            let body = format!(
                r#"<div class="rd" id="ffs-{id}">{head}<p class="small">{} / {}</p>{figs}</div>"#,
                link_to(&format!("rd-{id}"), "Reading"),
                link_to(&format!("sec-{id}"), "deep dive")
            );
            (id, body, names)
        })
        .collect();
    format!(
        r#"<p class="small muted">Built like Reading: sections in paper order, folded per chapter (first fold open), each section carrying all of its figures; a figure used in several sections is shown in each, with the same content. A term whose basis is not the paper itself is marked as such and unchecked.</p>{}"#,
        chapter_folds(r, items, "ffc")
    )
}

fn code_flow(r: &Record) -> Result<String> {
    let sections: Vec<_> = r
        .sections()
        .iter()
        .filter(|s| !blank(&s["steps"]))
        .collect();
    if sections.is_empty() {
        return Ok(r#"<p class="muted">(no code)</p>"#.into());
    }
    let mut items = Vec::new();
    let names: Vec<_> = sections
        .iter()
        .map(|s| format!("§{}", string(s, "id")))
        .collect();
    for (s, name) in sections.iter().zip(&names) {
        let id = string(s, "id");
        let prev = r
            .edges(id, true)
            .into_iter()
            .filter(|(o, _)| sections.iter().any(|s| string(s, "id") == *o))
            .map(|(o, w)| {
                format!(
                    "{}: {}",
                    link_to(&format!("cf-{o}"), &format!("§{o}")),
                    html(w)
                )
            })
            .collect::<Vec<_>>()
            .join("; ");
        let from = if prev.is_empty() {
            r#"<div class="muted small">(no direct link from another code section)</div>"#.into()
        } else {
            format!(r#"<div class="fromprev"><b>From the code of</b> {prev}</div>"#)
        };
        let kinds = active_code(s)
            .map(|c| {
                let lic = if blank(&c["lic"]) {
                    String::new()
                } else {
                    format!(", {}", eh(c, "lic"))
                };
                format!(
                    r#"{} {} <span class="muted small">[{}{lic}]</span>"#,
                    badge(string(c, "kind")),
                    eh(c, "name"),
                    eh(c, "state")
                )
            })
            .collect::<Vec<_>>()
            .join(" ");
        let terms = glossary(s, false);
        let terms = if terms.is_empty() {
            terms
        } else {
            format!(r#"<div class="figterms"><b>Terms of this section</b>{terms}</div>"#)
        };
        let body = format!(
            r#"<div class="figcard" id="cf-{id}"><h4>§{id} {}</h4>{from}<p>{kinds}</p>{}{terms}<p class="small">Occurs in: {}.</p></div>"#,
            eh(s, "title"),
            code_views(r, s, true)?,
            link_to(
                &format!("cu-{id}"),
                &format!(
                    "§{id}, code in the deep dive (the same two blocks plus the comparison table)"
                )
            )
        );
        items.push((id, body, vec![name.as_str()]));
    }
    Ok(format!(
        r#"<p class="small muted">Code sections in paper order, folded per chapter (first fold open). Each section carries its code twice: the whole code with nothing added, and the same code with a note above every line (function specs sit on the line that calls them).</p>{}"#,
        chapter_folds(r, items, "cfc")
    ))
}

fn build_html(r: &Record, regions: &Regions) -> Result<String> {
    let mut deep = String::new();
    for s in r.sections().iter().filter(|s| blank(&s["parent"])) {
        deep.push_str(&section_html(r, s, regions)?);
    }
    let figs: String = members(r.figures())
        .map(|(id, f)| {
            format!(
                r#"<tr id="fig-{}"><td>{}</td><td>p.{}</td><td>{}</td><td>{}</td></tr>"#,
                escape(id),
                escape(id),
                f["page"],
                mh(f, "what"),
                mh(f, "color")
            )
        })
        .collect();
    let codes: String = r
        .sections()
        .iter()
        .flat_map(|s| {
            active_code(s).map(move |c| {
                format!(
                    "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
                    section_link(string(s, "id")),
                    eh(c, "name"),
                    badge(string(c, "kind")),
                    eh(c, "src"),
                    eh(c, "state"),
                    eh(c, "lic")
                )
            })
        })
        .collect();
    let appendix_ledger = r.data["ledger_html"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| ledger::appendix(r));
    let appendix = [
        fold("figs", "blk mainfold", "Figure list", &format!("<table><tr><th>figure</th><th>page</th><th>content</th><th>colour legend</th></tr>{figs}</table>"), false),
        fold("codes", "blk mainfold", "Code list", &format!("<table><tr><th>section</th><th>code</th><th>kind</th><th>source</th><th>state</th><th>licence</th></tr>{codes}</table>"), false),
        fold("ledger", "blk mainfold", "Ledger (concepts, claims)", &fallback(appendix_ledger, "<p class=muted>(attach the ledger here)</p>"), false),
        r.data["appendix_html"].as_str().unwrap_or("<p class=muted>materials, acquisition state, request list</p>").to_owned(),
    ].concat();
    let body = [
        fold("reading", "blk mainfold", "Reading (original and faithful translation, section by section; the whole picture at the end)", &reading_html(r, regions), true),
        fold("figflow", "blk mainfold", "Figure flow (same order as Reading: per section, all of its figures; link from the previous figure, original, how to read it, colours, terms)", &figures_html(r), false),
        fold("codeflow", "blk mainfold", "Code flow (sections with code, in paper order)", &code_flow(r)?, false),
        fold("secs", "blk mainfold", "Deep dive (per section: what it says, original and translation, figures, code, terms, ledger)", &deep, false),
        fold("appx", "blk mainfold", "Appendix (indexes and materials)", &appendix, false),
    ].concat();
    let script = r#"<script>function ledAll(b,o){b.parentElement.parentElement.querySelectorAll('details.ledcard').forEach(function(d){d.open=o})}function openTo(h){var e=document.getElementById(h);while(e){if(e.tagName==='DETAILS')e.open=true;e=e.parentElement}}addEventListener('hashchange',function(){var h=location.hash.slice(1);if(h){openTo(h);var t=document.getElementById(h);if(t)t.scrollIntoView()}});if(location.hash)openTo(location.hash.slice(1));document.querySelectorAll('img[data-same]').forEach(function(im){var e=document.getElementById(im.dataset.same);var s=e&&e.querySelector('img');if(s&&s.src)im.src=s.src});</script>"#;
    let title = eh(&r.data, "title");
    Ok(format!(
        r#"<!doctype html><html><head><meta charset="utf-8"><title>{title}</title><style>{}</style></head><body><h1>{title}</h1><p class="small muted">{}. Private page; do not publish.</p>{body}{script}</body></html>"#,
        include_str!("assets/section.css").trim_end(),
        eh(&r.data, "source")
    ))
}

fn glossary_tex(s: &Value) -> String {
    let mut out = r"{\footnotesize\begin{longtable}{@{}p{2.8cm}p{9.2cm}p{1.8cm}p{1.2cm}@{}}\toprule term&explanation and use&where&field\\\midrule\endhead".to_owned();
    out.push('\n');
    for row in list(&s["glossary"]) {
        writeln!(
            out,
            r"{} & {} & {} & {} \\",
            tex(strval(&row[0])),
            tex(strval(&row[1])),
            tex(strval(&row[2])),
            tex(strval(&row[3]))
        )
        .unwrap();
    }
    out.push_str("\\bottomrule\\end{longtable}}\n");
    out
}

pub fn build(
    path: &Path,
    out: &Path,
    base: Option<&Path>,
    original: Option<&Path>,
) -> Result<bool> {
    let base = match base {
        Some(path) => path.to_owned(),
        None => path
            .canonicalize()?
            .parent()
            .context("record has no parent")?
            .to_owned(),
    };
    let record = Record {
        data: crate::support::json(path)?,
        base,
    };
    let issues = diagnose(&record);
    if !issues.errors.is_empty() {
        for error in issues.errors {
            println!("ERROR: {error}");
        }
        return Ok(false);
    }
    let page = build_html(&record, &original::regions(&record, original)?)?;
    std::fs::create_dir_all(out.join("panels"))?;
    std::fs::create_dir_all(out.join("codecards"))?;
    let mut tex = code::TeX {
        out,
        used: BTreeSet::new(),
        unknown: BTreeSet::new(),
    };
    for s in record.sections() {
        let id = string(s, "id");
        write(
            &out.join("panels").join(format!("p{id}.tex")),
            panel_tex(&record, s),
        )?;
        if !blank(&s["steps"]) {
            write(
                &out.join("codecards").join(format!("c{id}.tex")),
                tex.card(&record, s)?,
            )?;
        }
        if !blank(&s["glossary"]) {
            write(
                &out.join("glossary").join(format!("g{id}.tex")),
                glossary_tex(s),
            )?;
        }
        let ledger = ledger::section_tex(&record, s);
        if !ledger.is_empty() {
            write(&out.join("ledger").join(format!("l{id}.tex")), ledger)?;
        }
        if !blank(&s["bridge"]) {
            write(
                &out.join("bridge").join(format!("br{id}.tex")),
                format!(
                    "\\par\\medskip\\noindent{{\\small\\textbf{{To the next section.}} {}}}\\par\n",
                    tx(s, "bridge")
                ),
            )?;
        }
    }
    let definition = if tex.used.contains("julia") {
        crate::language::JULIA
    } else {
        ""
    };
    write(&out.join("lang_defs.tex"), format!("{definition}\n"))?;
    write(&out.join("index.html"), page)?;
    if !tex.unknown.is_empty() {
        println!(
            "WARNING: unknown code language (listed without color): {}",
            tex.unknown.into_iter().collect::<Vec<_>>().join(", ")
        );
    }
    println!(
        "built {} sections -> {}",
        record.sections().len(),
        out.display()
    );
    Ok(true)
}
