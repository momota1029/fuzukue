use serde_json::json;

use super::*;
use crate::language::{self, Language};

fn step_range(step: &Value) -> Option<(u64, u64)> {
    range(&json!([step["first"], step["last"]]))
}

fn language(step: &Value) -> Option<&'static Language> {
    language::of(string(step, "file"), string(step, "language"))
}

fn lang_key(lang: Option<&Language>) -> &str {
    lang.map_or("", |l| l.key)
}

fn lang_name(lang: Option<&Language>) -> &str {
    lang.map_or("(unknown language)", |l| l.name)
}

fn notes(section: &str, step: &Value, title: &str, d: &mut Diagnostics) {
    let rows = list(&step["lines"]);
    if rows.is_empty() {
        d.error(format!("{section}: step '{title}' has no line notes; every line needs one (grouping blank/import lines is fine)"));
        return;
    }
    let Some((first, last)) = step_range(step) else {
        d.error(format!("{section}: step '{title}': invalid line range"));
        return;
    };
    let mut coverage = Vec::new();
    for row in rows {
        let [a, b, text] = list(row) else {
            d.error(format!(
                "{section}: step '{title}': a line note must be [first, last, text]"
            ));
            return;
        };
        let (Some(a), Some(b)) = (a.as_u64(), b.as_u64()) else {
            d.error(format!(
                "{section}: step '{title}': a line note must be [first, last, text]"
            ));
            return;
        };
        if a > b || a < first || b > last {
            d.error(format!(
                "{section}: step '{title}': note range {a}-{b} is outside the Step {first}-{last}"
            ));
        }
        if strval(text).trim().chars().count() < 12 {
            d.warn(format!("{section}: step '{title}': note for lines {a}-{b} is too short to say what the lines are for"));
        }
        coverage.push((a, b));
    }
    coverage.sort_unstable();
    let mut next = Some(first);
    let covers = coverage.into_iter().all(|(a, b)| {
        let matches = next == Some(a) && a <= b;
        next = b.checked_add(1);
        matches
    });
    if !covers || next != last.checked_add(1) {
        d.error(format!(
            "{section}: step '{title}': notes must cover lines {first}-{last} exactly once"
        ));
    }
    if rows.len() as u64 * 2 < (last - first + 1) / 3 {
        d.warn(format!(
            "{section}: step '{title}': very few notes for {} lines",
            last - first + 1
        ));
    }
}

pub(super) fn diagnose(s: &Value, d: &mut Diagnostics) {
    let section = string(s, "id");
    let code = active_code(s).collect::<Vec<_>>();
    for c in list(&s["code"]) {
        let kind = string(c, "kind");
        if !KINDS.contains(&kind) {
            d.error(format!("{section}: code kind '{kind}'"));
        }
        if kind == "ai" && !truth(&c["labelled_standin"]) {
            d.error(format!(
                "{section}: AI code '{}' must be labelled as a stand-in",
                string(c, "name")
            ));
        }
    }
    if code.is_empty() {
        return;
    }
    let steps = list(&s["steps"]);
    if steps.is_empty() {
        d.error(format!("{section}: a code section needs Steps"));
    }
    if blank(&s["comparison"]) && blank(&s["no_comparison_reason"]) {
        d.error(format!(
            "{section}: needs a comparison or a reasoned 'no_comparison_reason'"
        ));
    }
    for key in ["flow", "why", "after"] {
        if blank(&s[key]) {
            d.warn(format!("{section}: code section lacks '{key}' (the goal of the code as a whole / why this tool and not another / what the result means)"));
        }
    }
    let mut alternatives = BTreeMap::<&str, BTreeSet<usize>>::new();
    for (index, step) in steps.iter().enumerate() {
        let title = string(step, "title");
        notes(section, step, title, d);
        let file = string(step, "file");
        if !file.is_empty() && language(step).is_none() {
            d.warn(format!("{section}: step '{title}': unknown code language for '{file}' (it is listed without color until then)"));
        }
        for key in ["title", "what", "file", "first", "last", "math"] {
            if step.get(key).is_none() {
                d.error(format!("{section}: step '{title}' lacks '{key}'"));
            }
        }
        for (i, alternative) in list(&step["also"]).iter().enumerate() {
            let tag = format!("{section}: step '{title}' also[{i}]");
            for key in ["file", "first", "last", "kind", "lines"] {
                if alternative.get(key).is_none() {
                    d.error(format!("{tag} lacks '{key}'"));
                }
            }
            let kind = string(alternative, "kind");
            if !KINDS.contains(&kind) {
                d.error(format!("{tag}: kind '{kind}'"));
            }
            let Some(file) = alternative["file"].as_str() else {
                continue;
            };
            let alt = language(alternative);
            alternatives
                .entry(alt.map_or(file, |lang| lang.key))
                .or_default()
                .insert(index);
            if alt.is_none() {
                d.warn(format!("{tag}: unknown code language for '{file}'"));
            }
            if lang_key(alt) == lang_key(language(step)) {
                d.warn(format!(
                    "{tag}: same language as the main version; a language gets one fold"
                ));
            }
            if kind == "port" && blank(&alternative["verified"]) {
                d.warn(format!("{tag}: AI port without 'verified' (state how it was checked against the original, or say it was not)"));
            }
            if ["first", "last", "lines"]
                .iter()
                .all(|key| alternative.get(key).is_some())
            {
                notes(section, alternative, title, d);
            }
        }
    }
    for (key, seen) in alternatives {
        let missing: Vec<_> = steps
            .iter()
            .enumerate()
            .filter(|(i, _)| !seen.contains(i))
            .map(|(_, step)| string(step, "title"))
            .collect();
        if !missing.is_empty() {
            d.warn(format!("{section}: language '{key}' is missing for Steps {missing:?}; each fold shows 'no version of this Step' there"));
        }
    }
    for row in list(&s["comparison"]["rows"]) {
        if status(string(row, "status")).is_none() {
            d.error(format!(
                "{section}: comparison row status '{}'",
                string(row, "status")
            ));
        }
        if blank(&row["why"]) {
            d.error(format!(
                "{section}: comparison row '{}' lacks a reason",
                string(row, "label")
            ));
        }
    }
}

fn status(key: &str) -> Option<(&str, &str)> {
    Some(match key {
        "same" => ("same", "same"),
        "changed" => ("changed", "chg"),
        "only_left" => ("only left", "onl"),
        "only_right" => ("only right", "onl"),
        _ => return None,
    })
}

fn span(value: &Value) -> Option<(&str, u64, u64)> {
    let [file, a, b] = list(value) else {
        return None;
    };
    let (file, a, b) = (file.as_str()?, a.as_u64()?, b.as_u64()?);
    (a >= 1 && a <= b).then_some((file, a, b))
}

fn views(step: &Value) -> Vec<Value> {
    std::iter::once(step.clone())
        .chain(list(&step["also"]).iter().map(|alt| {
            let mut view = with(
                step,
                [
                    ("apis", json!([])),
                    ("lines", json!([])),
                    ("language", Value::Null),
                ],
            );
            if let Some(fields) = view.as_object_mut() {
                fields.extend(members(alt).map(|(k, v)| (k.clone(), v.clone())));
            }
            view
        }))
        .collect()
}

fn groups(s: &Value) -> Vec<Option<&'static Language>> {
    let mut result = Vec::new();
    let mut seen = BTreeSet::new();
    for lang in list(&s["steps"])
        .iter()
        .flat_map(views)
        .map(|s| language(&s))
    {
        if seen.insert(lang_key(lang)) {
            result.push(lang);
        }
    }
    result
}

fn view_in(step: &Value, lang: Option<&Language>) -> Option<Value> {
    views(step)
        .into_iter()
        .find(|s| lang_key(language(s)) == lang_key(lang))
}

fn numbered(lines: &[String], first: u64) -> String {
    lines
        .iter()
        .enumerate()
        .map(|(i, line)| {
            format!(
                r#"<span class="ln">{}</span> {}"#,
                first + i as u64,
                escape(line)
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn code_box(r: &Record, side: &Value, class: &str) -> Result<String> {
    if let Some(lit) = side["lit"].as_str() {
        return Ok(format!(
            r#"<pre class="cb {class}"><span class="ln">—</span> {}</pre>"#,
            escape(lit)
        ));
    }
    if blank(side) {
        return Ok(r#"<pre class="cb none">(not on this side)</pre>"#.into());
    }
    let (f, a, b) = span(side).context("comparison line span must be [file, first, last]")?;
    let source = numbered(&r.excerpt(f, a, b)?, a);
    Ok(format!(
        r#"<pre class="cb {class}">{source}</pre><div class="muted small">{} {a}–{b}</div>"#,
        escape(f)
    ))
}

pub(super) fn summary(cmp: &Value) -> String {
    let mut counts: BTreeMap<&str, u64> = BTreeMap::new();
    for row in list(&cmp["rows"]) {
        *counts.entry(string(row, "status")).or_default() +=
            span(&row["right"]).map_or(0, |(_, a, b)| b - a + 1);
    }
    let total: u64 = counts.values().sum();
    let n = |key| counts.get(key).copied().unwrap_or_default();
    format!("Of the {total} right-hand lines compared, {} are the same, {} changed, {} only on the right.",
        n("same"), n("changed"), n("only_right"))
}

fn compare_notes(cmp: &Value, file: &str, a: u64, b: u64) -> String {
    let mut out = String::new();
    for row in list(&cmp["rows"]) {
        for (mine, other) in [("left", "right"), ("right", "left")] {
            if !span(&row[mine])
                .is_some_and(|(f, first, last)| f == file && a <= last && first <= b)
            {
                continue;
            }
            let (label, class) = status(string(row, "status")).unwrap_or(("?", ""));
            let other_name = mh(&cmp[other], "name");
            let counterpart = if let Some((f, a, b)) = span(&row[other]) {
                format!("{other_name}, <code>{}</code> lines {a}-{b}", escape(f))
            } else if let Some(lit) = row[other]["lit"].as_str() {
                format!("in {other_name}: one line <code>{}</code>", escape(lit))
            } else {
                format!("{other_name} has no corresponding lines")
            };
            write!(out, r#"<div class="cnote {class}"><span class="stl {class}">{label}</span> <b>{}</b>. Counterpart: {counterpart}. {}</div>"#,
                eh(row, "label"), mh(row, "why")).unwrap();
            break;
        }
    }
    out
}

fn matching_api(lines: &[String], lang: Option<&Language>, name: &str) -> bool {
    let source = lines.join("\n");
    if let Some(lang) = lang {
        return lang.calls(&source, name);
    }
    let pattern = format!(r"(?<![\w.]){}\s*\(", regex::escape(name));
    let source = lines
        .iter()
        .map(|line| {
            line.split(['#', '%'])
                .next()
                .unwrap_or_default()
                .split("//")
                .next()
                .unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join("\n");
    fancy_regex::Regex::new(&pattern).is_ok_and(|re| re.is_match(&source).unwrap_or(false))
}

fn api_card(api: &Value) -> String {
    let mut card = format!(
        r#"<div class="apic"><b><code>{}</code></b> <code class="small">{}</code><br><b>returns:</b> {}<br><b>note:</b> {}"#,
        eh(api, "name"),
        eh(api, "sig"),
        mh(api, "ret"),
        mh(api, "note")
    );
    if !blank(&api["cost"]) {
        write!(card, "<br><b>cost:</b> {}", mh(api, "cost")).unwrap();
    }
    if !blank(&api["quote"]) {
        write!(
            card,
            r#"<div class="small muted">{}</div>"#,
            eh(api, "quote")
        )
        .unwrap();
    }
    card.push_str("</div>");
    card
}

fn lang_header(lang: Option<&Language>, main: Option<&Language>, view: &Value) -> String {
    let mut bits = Vec::new();
    if let Some(lang) = lang {
        let mut base = format!("first index {}", lang.index_base);
        let mut order = format!("matrices {}-major", lang.order);
        if let Some(main) = main {
            if main.index_base != lang.index_base {
                write!(
                    base,
                    " (the {} version starts at {})",
                    main.name, main.index_base
                )
                .unwrap();
            }
            if main.order != lang.order {
                write!(
                    order,
                    " (the {} version is {}-major)",
                    main.name, main.order
                )
                .unwrap();
            }
        }
        bits.push(format!("{base}; {order}"));
        bits.push(format!("traps: {}", escape(&lang.notes.join(" "))));
    }
    if string(view, "kind") == "port" {
        bits.push(format!(
            "checked against the original: {}",
            fallback(eh(view, "verified"), "<b>not checked</b>")
        ));
    }
    if bits.is_empty() {
        String::new()
    } else {
        format!(
            r#"<p class="small muted langinfo">{}</p>"#,
            bits.join(" · ")
        )
    }
}

pub(super) fn steps_html(r: &Record, s: &Value, annotated: bool) -> Result<String> {
    let groups = groups(s);
    let mut result = String::new();
    for (i, &lang) in groups.iter().enumerate() {
        let main = i == 0;
        let name = lang_name(lang);
        let mut rendered = String::new();
        let mut seen = BTreeSet::new();
        let mut kinds = BTreeSet::new();
        let mut first_view = Value::Null;
        for step in list(&s["steps"]) {
            let Some(view) = view_in(step, lang) else {
                write!(rendered, r#"<div class="step"><h4>{}</h4><p class="small muted">No {} version of this Step.</p></div>"#,
                    mh(step, "title"), escape(name)).unwrap();
                continue;
            };
            if first_view.is_null() {
                first_view = view.clone();
            }
            let step_kind = string(&view, "kind");
            if !step_kind.is_empty() {
                kinds.insert(kind(step_kind).to_owned());
            }
            let file = string(&view, "file");
            let (a, b) = step_range(&view).context("Step has an invalid range")?;
            let lines = r.excerpt(file, a, b)?;
            let name = escape(name);
            if !annotated {
                write!(rendered, r#"<div class="step plain"><h4>{}</h4><pre class="cb">{}</pre><div class="muted small">{} {a}-{b} · {name}</div></div>"#,
                    eh(step, "title"), numbered(&lines, a), escape(file)).unwrap();
                continue;
            }
            let apis = list(&view["apis"]);
            for api in apis {
                let name = string(api, "name");
                let name = lang
                    .and_then(|language| name.strip_prefix(language.api_prefix))
                    .unwrap_or(name);
                if !matching_api(&lines, lang, name) {
                    eprintln!("warning: {}: step '{}': the spec of '{name}' matches no call on the Step's lines, so it is not shown",
                        string(s, "id"), string(step, "title"));
                }
            }
            write!(
                rendered,
                r#"<div class="step"><h4>{}</h4><p>{}</p><div class="acode">"#,
                mh(step, "title"),
                mh(step, "what")
            )
            .unwrap();
            for note in list(&view["lines"]) {
                let from = note[0].as_u64().context("line note needs its first line")?;
                let to = note[1].as_u64().context("line note needs its last line")?;
                ensure!(a <= from && from <= to && to <= b, "line note outside Step");
                let lines = &lines[(from - a) as usize..=(to - a) as usize];
                let label = if from == to {
                    format!("line {from}")
                } else {
                    format!("lines {from}-{to}")
                };
                write!(rendered, r#"<div class="ablk"><div class="anote"><span class="alab">{label}</span> {}</div>"#, html(strval(&note[2]))).unwrap();
                if main {
                    rendered.push_str(&compare_notes(&s["comparison"], file, from, to));
                }
                for api in apis {
                    let name = string(api, "name");
                    let name = lang
                        .and_then(|language| name.strip_prefix(language.api_prefix))
                        .unwrap_or(name);
                    if matching_api(lines, lang, name) {
                        if seen.insert(name.to_owned()) {
                            rendered.push_str(&api_card(api));
                        } else {
                            write!(rendered, r#"<div class="apic small muted"><code>{}</code>: same as above (first call).</div>"#, escape(name)).unwrap();
                        }
                    }
                }
                write!(
                    rendered,
                    r#"<pre class="cb">{}</pre></div>"#,
                    numbered(lines, from)
                )
                .unwrap();
            }
            write!(rendered, r#"</div><div class="muted small">{} {a}-{b} · {name} (notes are display only; the file is unchanged)</div>"#, escape(file)).unwrap();
            if !main {
                rendered.push_str(r#"<p class="small muted">Comparison counterparts are shown in the main-language version.</p>"#);
            }
            write!(
                rendered,
                r#"<p class="mapto"><b>Math:</b> {}</p></div>"#,
                mh(step, "math")
            )
            .unwrap();
        }
        if groups.len() == 1 {
            return Ok(rendered);
        }
        let open = if main { " open" } else { "" };
        let kind = if kinds.is_empty() {
            String::new()
        } else {
            format!(" · {}", kinds.into_iter().collect::<Vec<_>>().join(", "))
        };
        write!(result, r#"<details class="langfold"{open}><summary><b>{}</b>{}</summary><div class="inner">{}{rendered}</div></details>"#,
            escape(name), escape(&kind), lang_header(lang, groups[0], &first_view)).unwrap();
    }
    Ok(result)
}

pub(super) fn comparison_html(r: &Record, section: &Value) -> Result<String> {
    let cmp = &section["comparison"];
    if blank(cmp) {
        return Ok(format!(
            r#"<div class="cmpcard none"><b>No comparison.</b> {}</div>"#,
            mh(section, "no_comparison_reason")
        ));
    }
    let mut out = format!(
        r#"<div class="cmpcard"><h3>{}</h3><p><b>{}</b></p>"#,
        mh(cmp, "title"),
        summary(cmp)
    );
    for row in list(&cmp["rows"]) {
        let (label, class) = status(string(row, "status")).context("invalid comparison status")?;
        write!(out, r#"<div class="cmprow {class}"><div><span class="stl {class}">{label}</span> <b>{}</b>: {}</div><div class="cmpgrid">"#,
            eh(row, "label"), mh(row, "why")).unwrap();
        for (side, text) in [("left", "Left"), ("right", "Right")] {
            let value = &row[side];
            write!(
                out,
                r#"<div><div class="side">{text}: {}</div>{}</div>"#,
                mh(&cmp[side], "name"),
                code_box(r, value, class)?
            )
            .unwrap();
        }
        out.push_str("</div></div>");
    }
    out.push_str("<h4>Parameters</h4><table><tr><th>argument</th><th>left</th><th>right</th><th>effect</th></tr>");
    for row in list(&cmp["params"]) {
        out.push_str("<tr>");
        for cell in list(row) {
            write!(out, "<td>{}</td>", html(strval(cell))).unwrap();
        }
        out.push_str("</tr>");
    }
    write!(
        out,
        r#"</table><p class="small">{}</p></div>"#,
        mh(cmp, "note")
    )
    .unwrap();
    Ok(out)
}

pub(super) struct TeX<'a> {
    pub out: &'a Path,
    pub used: BTreeSet<&'static str>,
    pub unknown: BTreeSet<String>,
}

impl TeX<'_> {
    fn listing(&mut self, r: &Record, excerpt: (&str, u64, u64), override_key: &str) -> String {
        let (file, a, b) = excerpt;
        let lang = language::of(file, override_key);
        let option = if let Some(lang) = lang {
            self.used.insert(lang.key);
            format!("language={},", lang.listings)
        } else {
            self.unknown.insert(file.into());
            String::new()
        };
        format!(
            r"\lstinputlisting[{option}firstline={a},lastline={b},firstnumber={a}]{{{}}}\par{{\scriptsize {} {a}--{b}, {}}}",
            r.base.join(file).display(),
            tex(file),
            tex(lang_name(lang))
        )
    }

    fn side(&mut self, r: &Record, value: &Value, tag: &str) -> Result<String> {
        if let Some(lit) = value["lit"].as_str() {
            let path = self.out.join("lit").join(format!("{tag}.txt"));
            write(&path, format!("{lit}\n"))?;
            return Ok(format!(
                r"\lstinputlisting[numbers=none]{{{}}}",
                path.display()
            ));
        }
        match span(value) {
            Some(range) => Ok(self.listing(r, range, "")),
            None if blank(value) => Ok(r"\par{\small\itshape (not on this side)}".into()),
            None => anyhow::bail!("comparison line span must be [file, first, last]"),
        }
    }

    pub(super) fn card(&mut self, r: &Record, s: &Value) -> Result<String> {
        let id = string(s, "id");
        let cmp = &s["comparison"];
        let compare = !blank(cmp);
        let suffix = if compare {
            " and comparison"
        } else {
            " (no comparison)"
        };
        let mut out =
            format!(r"\par\medskip\noindent{{\large\bfseries \S{id} Code: Steps{suffix}}}\par");
        out.push('\n');
        if !blank(&s["flow"]) {
            let why = if blank(&s["why"]) {
                String::new()
            } else {
                format!(r"\par {}", tx(s, "why"))
            };
            writeln!(out, r"\par{{\small\textbf{{Goal of this code as a whole, and why this tool.}} {}{why}}}\par", tx(s, "flow")).unwrap();
        }
        let groups = groups(s);
        for (annotated, heading) in [
            (false, "The code as a whole, with nothing added"),
            (true, "The same code with a note above every line"),
        ] {
            writeln!(out, r"\par\smallskip\noindent\textbf{{{heading}}}\par").unwrap();
            for &lang in &groups {
                let name = lang_name(lang);
                if groups.len() > 1 {
                    writeln!(
                        out,
                        r"\par\smallskip\noindent\textsc{{Language: {name}}}\par"
                    )
                    .unwrap();
                }
                for step in list(&s["steps"]) {
                    let title = tx(step, "title");
                    let Some(view) = view_in(step, lang) else {
                        if !annotated {
                            writeln!(
                                out,
                                r"\par\smallskip\noindent\textbf{{{title}}}\ (no {name} version)"
                            )
                            .unwrap();
                        }
                        continue;
                    };
                    let file = string(&view, "file");
                    let (first, last) = step_range(&view).context("invalid Step range")?;
                    let override_key = string(&view, "language");
                    if !annotated {
                        writeln!(out, r"\par\smallskip\noindent\textbf{{{title}}}").unwrap();
                        writeln!(
                            out,
                            "{}",
                            self.listing(r, (file, first, last), override_key)
                        )
                        .unwrap();
                        continue;
                    }
                    writeln!(
                        out,
                        r"\par\smallskip\noindent\textbf{{{title}}}\quad {}\par",
                        tx(step, "what")
                    )
                    .unwrap();
                    for note in list(&view["lines"]) {
                        let a = note[0].as_u64().context("invalid note line")?;
                        let b = note[1].as_u64().context("invalid note line")?;
                        let label = if a == b {
                            format!("line {a}")
                        } else {
                            format!("lines {a}--{b}")
                        };
                        writeln!(
                            out,
                            r"\par\smallskip{{\footnotesize\textbf{{{label}:}} {}}}",
                            tex(strval(&note[2]))
                        )
                        .unwrap();
                        writeln!(out, "{}", self.listing(r, (file, a, b), override_key)).unwrap();
                    }
                    let apis = list(&view["apis"]);
                    if !apis.is_empty() {
                        out.push_str(
                            r"\par{\small\textbf{Functions used}}\begin{itemize}\footnotesize",
                        );
                        for api in apis {
                            write!(
                                out,
                                r"\item \texttt{{{}}}\ \texttt{{{}}}: returns {}; note: {}",
                                tx(api, "name"),
                                tx(api, "sig"),
                                tx(api, "ret"),
                                tx(api, "note")
                            )
                            .unwrap();
                            if !blank(&api["quote"]) {
                                write!(out, r" {{\scriptsize {}}}", tx(api, "quote")).unwrap();
                            }
                        }
                        out.push_str("\\end{itemize}\n");
                    }
                    writeln!(
                        out,
                        r"\par{{\small\textbf{{Math:}} {}}}\par",
                        tx(step, "math")
                    )
                    .unwrap();
                }
            }
        }
        if !blank(&s["after"]) {
            writeln!(
                out,
                r"\par{{\small\textbf{{Meaning of the result.}} {}}}\par",
                tx(s, "after")
            )
            .unwrap();
        }
        if compare {
            writeln!(
                out,
                r"\par\medskip\noindent\textbf{{{}}}\par{{\small {}}}\par",
                tx(cmp, "title"),
                tex(&summary(cmp))
            )
            .unwrap();
            for (i, row) in list(&cmp["rows"]).iter().enumerate() {
                let (label, color) = match status(string(row, "status")) {
                    Some((label, "same")) => (label, "green!12"),
                    Some((label, "chg")) => (label, "yellow!25"),
                    Some((label, _)) => (label, "blue!10"),
                    None => anyhow::bail!("invalid comparison status"),
                };
                let tag = format!("{}_{i}", id.replace('.', "_"));
                let left = self.side(r, &row["left"], &format!("{tag}_L"))?;
                let right = self.side(r, &row["right"], &format!("{tag}_R"))?;
                writeln!(out, r"\begin{{tcolorbox}}[colback={color},boxrule=0.3pt,fontupper=\small]\textbf{{[{label}] {}:}} {}\par\noindent\begin{{minipage}}[t]{{0.485\linewidth}}{left}\end{{minipage}}\hfill\begin{{minipage}}[t]{{0.485\linewidth}}{right}\end{{minipage}}\end{{tcolorbox}}",
                    tx(row, "label"), tx(row, "why")).unwrap();
            }
            writeln!(out, r"\par{{\small {}}}", tx(cmp, "note")).unwrap();
        } else {
            writeln!(
                out,
                r"\par{{\small\textbf{{No comparison.}} {}}}",
                tx(s, "no_comparison_reason")
            )
            .unwrap();
        }
        Ok(out)
    }
}
