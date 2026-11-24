use super::*;
use std::fmt::Write as _;

const RELATIONS: &[&str] = &["same", "changed", "author-only", "prior-only"];
const PROVENANCE: &[&str] = &[
    "AI re-implementation",
    "author-code run",
    "earlier-method run",
];

fn vstr(value: &Value) -> String {
    if let Some(s) = value.as_str() {
        return s.into();
    }
    match value {
        Value::Array(items) => format!(
            "[{}]",
            items.iter().map(vstr_json).collect::<Vec<_>>().join(", ")
        ),
        Value::Object(items) => format!(
            "{{{}}}",
            items
                .iter()
                .map(|(k, v)| format!("{}: {}", ascii_json_string(k), vstr_json(v)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Value::Number(number) if number.is_f64() => {
            let number = number.as_f64().unwrap();
            let magnitude = number.abs();
            if magnitude != 0.0 && !(1e-4..1e16).contains(&magnitude) {
                let scientific = format!("{number:e}");
                let (mantissa, exponent) = scientific.split_once('e').unwrap();
                let exponent: i32 = exponent.parse().unwrap();
                format!("{mantissa}e{exponent:+03}")
            } else {
                format!("{number:?}")
            }
        }
        _ => value.to_string(),
    }
}

fn vstr_json(value: &Value) -> String {
    if let Some(value) = value.as_str() {
        ascii_json_string(value)
    } else {
        vstr(value)
    }
}

// Stored result keys use Python's json.dumps spelling, including ASCII escapes
// inside structured values. Top-level string controls use the string verbatim.
fn ascii_json_string(value: &str) -> String {
    let quoted = json!(value).to_string();
    let mut ascii = String::with_capacity(quoted.len());
    for unit in quoted.encode_utf16() {
        if unit < 0x7f {
            ascii.push(char::from(unit as u8));
        } else {
            write!(ascii, r"\u{unit:04x}").unwrap();
        }
    }
    ascii
}

fn grid_keys(params: &[Value], results: &Value, side: &str, report: &mut Report) {
    fn visit(
        prefix: &mut String,
        params: &[Value],
        results: &Value,
        side: &str,
        report: &mut Report,
    ) {
        let Some((param, rest)) = params.split_first() else {
            let key = if prefix.is_empty() {
                "default"
            } else {
                prefix.as_str()
            };
            if results.get(key).is_none() {
                report(format!("{side} grid point '{key}' missing in results"));
            }
            return;
        };
        let len = prefix.len();
        for point in list(&param["grid"]) {
            if len > 0 {
                prefix.push('|');
            }
            prefix.push_str(string(param, "name"));
            prefix.push('=');
            prefix.push_str(&vstr(point));
            visit(prefix, rest, results, side, report);
            prefix.truncate(len);
        }
    }
    visit(&mut String::new(), params, results, side, report);
}

fn params(
    code: &Value,
    slot: &str,
    lines: &[String],
    terms: &[Value],
    report: &mut Report,
) -> Value {
    let known = |name: &str| {
        !name.is_empty() && terms.iter().any(|term| string(term, "term").contains(name))
    };
    let params: Vec<_> = list(&code["params"]).iter().map(|param| {
        let name = string(param, "name");
        let mut report = |msg| report(format!("{slot} param {name}: {msg}"));
        if !known(name) {
            report("not defined in the unit's terms (define before use)".into());
        }
        let data = truth(&param["data"]);
        if !data && (blank(&param["label"]) || !string(param, "label").contains(name)) {
            report("label (the control's name: the code's argument and what it sets in the paper's terms) is missing or does not contain the argument".into());
        }
        let derives = &param["derives"];
        if !derives.is_null() {
            let name = string(derives, "name");
            if !fields_are_present(derives, &["name", "key"]) {
                report("derives needs name (the paper's symbol) and key (in the results)".into());
            } else if !known(name) {
                report(format!("derived symbol '{name}' not defined in the unit's terms"));
            }
        }
        let grid = list(&param["grid"]);
        let valid_default = grid.contains(&param["default"]);
        if data {
            if blank(&param["effect"]) || !valid_default {
                report("needs effect and a default in its grid".into());
            }
        } else {
            let line = param["line"].as_u64().unwrap_or(0);
            if !in_range(&json!([line, line]), code) {
                report(format!("line {line} outside the excerpt"));
            } else {
                let token = default(param, "token", "\0");
                if !lines.get(line as usize - 1).is_some_and(|line| line.contains(token)) {
                    report(format!("token '{token}' not on line {line}"));
                }
            }
            if !valid_default { report("default not in grid".into()); }
            if !matches!(string(param, "stated_in"), "paper" | "code") {
                report("stated_in must be 'paper' or 'code'".into());
            }
            if blank(&param["effect"]) { report("missing effect".into()); }
        }
        with(param, [("grid_str", json!(grid.iter().map(vstr).collect::<Vec<_>>())),
            ("default_str", json!(vstr(&param["default"])))])
    }).collect();
    json!(params)
}

fn presets(presets: &Value, code: &Value, label: &str, report: &mut Report) {
    let params = list(&code["params"]);
    if !params.is_empty() && blank(presets) {
        report(format!(
            "no {label} (name values, including extreme ones, and what should visibly change)"
        ));
    }
    for preset in list(presets) {
        let title = string(preset, "label");
        if !fields_are_present(preset, &["label", "expect", "values"]) {
            report(format!("{label} '{title}': needs label, values and expect"));
            continue;
        }
        for (name, value) in members(&preset["values"]) {
            match params.iter().find(|p| string(p, "name") == name) {
                None => report(format!("{label} '{title}': unknown parameter '{name}'")),
                Some(param) if !list(&param["grid"]).iter().any(|v| vstr(v) == vstr(value)) => {
                    report(format!(
                        "{label} '{title}': {name}={} is not a grid point",
                        vstr(value)
                    ));
                }
                _ => (),
            }
        }
    }
}

fn code(
    ctx: &Spec,
    excerpt: &Value,
    terms: &[Value],
    slot: &str,
    report: &mut Report,
) -> Option<Value> {
    if excerpt.is_string() {
        if !na(excerpt) {
            report(format!("{slot} must be an excerpt or 'n/a: reason'"));
        }
        return Some(json!({"na": excerpt}));
    }
    if blank(excerpt) {
        report(format!(
            "missing {slot} (use 'n/a: reason' when none applies)"
        ));
        return None;
    }
    let id = default(
        excerpt,
        "impl",
        if slot == "author_code" { "author" } else { "" },
    );
    let Some(method) = ctx.methods.get(id).filter(|v| !blank(v)) else {
        report(format!("{slot}: unknown impl '{id}'"));
        return None;
    };
    let (text, lines) = match source(&ctx.base.join(string(method, "root")), excerpt) {
        Ok(source) => source,
        Err(error) => {
            report(format!("{slot}: cannot read excerpt ({error})"));
            return None;
        }
    };
    let roles = list(&excerpt["roles"]);
    let folds = list(&excerpt["fold"]);
    for role in roles {
        if !in_range(&json!([role["first"], role["last"]]), excerpt) || blank(&role["text"]) {
            report(format!(
                "{slot}: role/fold block {}-{} outside the excerpt or without text",
                role["first"], role["last"]
            ));
        }
    }
    for fold in folds {
        if !in_range(fold, excerpt) {
            report(format!("{slot}: fold block {fold} outside the excerpt"));
        }
    }
    if slot == "prior_code" && blank(&excerpt["failure"]) {
        report(
            "prior_code needs where the earlier method fails or differs on the same input".into(),
        );
    }
    Some(with(
        &selected(method, &["repo", "url", "commit", "license"]),
        [
            ("impl", method["name"].clone()),
            ("source", json!(default(method, "source", ""))),
            ("file", excerpt["file"].clone()),
            ("first", excerpt["first"].clone()),
            ("last", excerpt["last"].clone()),
            ("sha256", json!(sha(&text))),
            ("text", json!(text)),
            ("failure", json!(default(excerpt, "failure", ""))),
            ("roles", json!(roles)),
            ("fold", json!(folds)),
            ("link", json!(truth(&excerpt["link"]))),
            ("params", params(excerpt, slot, &lines, terms, report)),
        ],
    ))
}

fn comparison(feature: &Value, author: &Value, prior: &Value, report: &mut Report) {
    if truth(&prior["link"]) && list(&prior["params"]).len() != list(&author["params"]).len() {
        report("prior_code link needs as many earlier-method parameters as authors' ones".into());
    }
    let rows = list(&feature["comparison"]);
    if rows.is_empty() {
        report(
            "no comparison list (pairs of line ranges: same / changed / author-only / prior-only)"
                .into(),
        );
    }
    for (i, row) in rows.iter().enumerate() {
        let index = i + 1;
        let rel = string(row, "relation");
        if !RELATIONS.contains(&rel) {
            report(format!(
                "comparison {index}: relation must be one of {RELATIONS:?}"
            ));
        }
        for (key, why) in [
            ("does", "what the code does, in plain words"),
            (
                "lineage",
                "where it comes from: inherited from which work, changed, added, not used",
            ),
            ("text", "what differs and what that changes in the output"),
        ] {
            if blank(&row[key]) {
                report(format!("comparison {index}: missing {key} ({why}); the relation label only summarises it"));
            }
        }
        let needs_author = matches!(rel, "same" | "changed" | "author-only");
        let needs_prior = matches!(rel, "same" | "changed" | "prior-only");
        if needs_author == row["author"].is_null() || needs_prior == row["prior"].is_null() {
            report(format!(
                "comparison {index}: ranges do not fit the relation '{rel}'"
            ));
        }
        for (side, label, code) in [("author", "author", author), ("prior", "earlier", prior)] {
            if !row[side].is_null() && !in_range(&row[side], code) {
                report(format!(
                    "comparison {index}: {label} range {} outside the excerpt",
                    row[side]
                ));
            }
        }
    }
    for role in list(&author["roles"]) {
        let first = role["first"].as_u64().unwrap_or(0);
        let last = role["last"].as_u64().unwrap_or(0);
        if !rows
            .iter()
            .filter_map(|row| line_range(&row["author"]))
            .any(|(a, b)| a <= last && first <= b)
        {
            report(format!(
                "comparison: authors' block L{first}-{last} has no pair or only-entry"
            ));
        }
    }
}

fn results(
    ctx: &Spec,
    feature: &Value,
    author: &Value,
    prior: &Value,
    report: &mut Report,
) -> Value {
    let path = string(feature, "results");
    let results = match crate::support::json(&ctx.base.join(path)) {
        Ok(results) if results.is_object() => results,
        Err(error) if ctx.base.join(path).is_file() => {
            report(format!("results file is invalid: {error:#}"));
            return json!({});
        }
        _ => {
            report(format!(
                "results file missing: {}",
                if path.is_empty() { "None" } else { path }
            ));
            return json!({});
        }
    };
    let prov = string(&results, "provenance");
    if !PROVENANCE.contains(&prov) {
        report("results provenance must be one of ['AI re-implementation', 'author-code run', 'earlier-method run']".into());
    }
    for key in ["script", "commit", "data"] {
        if blank(&results[key]) {
            report(format!("results lack {key}"));
        }
    }
    let runtime = results["runtime_seconds"].as_f64();
    if runtime.is_none() {
        report("results lack runtime_seconds (the time the longest run took)".into());
    }
    if results.get("scale_down").is_none() {
        report(
            "results lack scale_down (what is smaller than in the paper; use '' when nothing)"
                .into(),
        );
    } else if let Some(rt) = runtime.filter(|rt| *rt > 300.0 && blank(&results["over_budget"])) {
        report(format!("runtime {rt:.0} s is over the 5-minute budget: say why in over_budget (e.g. kept because already finished)"));
    }
    let parity = &results["parity"];
    if prov == "AI re-implementation"
        && (blank(&parity["against"])
            || !parity["max_abs_diff"]
                .as_f64()
                .zip(parity["tolerance"].as_f64())
                .is_some_and(|(diff, tolerance)| diff <= tolerance))
    {
        report("AI re-implementation without a passing parity record is not shown".into());
    }
    grid_keys(
        list(&author["params"]),
        &results["author"],
        "author",
        report,
    );
    for (side, code) in [("author", author), ("prior", prior)] {
        for param in list(&code["params"]) {
            let key = string(&param["derives"], "key");
            if !key.is_empty() && members(&results[side]).any(|(_, point)| point.get(key).is_none())
            {
                report(format!(
                    "{side} param {}: derived key '{key}' missing in some {side} grid points",
                    string(param, "name")
                ));
            }
        }
    }
    if prior["text"].is_string() {
        grid_keys(
            list(&prior["params"]),
            &results["prior"],
            "earlier-method",
            report,
        );
    }
    results
}

fn run_card(ctx: &Spec, card: &Value, author: &Value, prior: &Value, report: &mut Report) {
    if !card.is_object() {
        report("missing run_card (calls, flow, steps, not_run): say what was run".into());
        return;
    }
    let calls = list(&card["calls"]);
    if calls.is_empty() {
        report("run_card: no calls".into());
    }
    for (i, call) in calls.iter().enumerate() {
        if !fields_are_present(call, &["code", "where"])
            || !matches!(string(call, "by"), "author" | "earlier" | "AI")
        {
            report(format!(
                "run_card call {}: needs code, where and by ('author', 'earlier' or 'AI')",
                i + 1
            ));
        }
    }
    if !calls.iter().any(|c| string(c, "by") == "author") {
        report("run_card: no call of the authors' code".into());
    }
    for (side, code) in [("author", author), ("prior", prior)] {
        for p in list(&code["params"]).iter().filter(|p| !truth(&p["data"])) {
            let name = string(p, "name");
            let mut flows = list(&card["flow"])
                .iter()
                .filter(|f| string(f, "control") == name && default(f, "side", "author") == side)
                .peekable();
            if flows.peek().is_none() {
                report(format!("run_card: no flow for the {side} control {name} (argument → line → output → drawn)"));
            }
            for flow in flows {
                need(flow, &["argument", "output", "drawn"], &mut |msg| {
                    report(format!("run_card flow {name}: {msg}"))
                });
                let line = flow["line"].as_u64().unwrap_or(0);
                if !in_range(&json!([line, line]), code) {
                    report(format!("run_card flow {name}: line {line} outside the {side} excerpt (the line is re-shown next to the run)"));
                }
            }
        }
    }
    let steps = list(&card["steps"]);
    if steps.is_empty() || steps.iter().any(|step| !ctx.steps.contains(strval(step))) {
        report("run_card: steps must list the overview steps the run executes".into());
    }
    if blank(&card["not_run"]) {
        report(
            "run_card: not_run (which steps or panels the run does not execute) is missing".into(),
        );
    }
}

fn called(ctx: &Spec, feature: &Value, report: &mut Report) -> (Value, Value) {
    let out: Vec<_> = list(&feature["called"])
        .iter()
        .filter_map(|code| {
            let name = string(code, "name");
            let method = ctx.methods.get(string(code, "impl"));
            let Some(method) = method.filter(|_| !name.is_empty()) else {
                report(format!(
                    "called '{name}': needs name and impl (a method id)"
                ));
                return None;
            };
            match source(&ctx.base.join(string(method, "root")), code) {
                Ok((text, _)) => Some(with(
                    code,
                    [
                        ("text", json!(text)),
                        ("repo", method["repo"].clone()),
                        ("url", method["url"].clone()),
                        ("commit", method["commit"].clone()),
                        ("license", method["license"].clone()),
                        ("impl_name", method["name"].clone()),
                    ],
                )),
                Err(error) => {
                    report(format!("called {name}: cannot read ({error})"));
                    None
                }
            }
        })
        .collect();
    let sell: Vec<_> = list(&feature["sell"])
        .iter()
        .filter(|s| {
            let (Some(a), Some(b)) = (s["first"].as_u64(), s["last"].as_u64()) else {
                report("sell needs label, file, first and last".into());
                return false;
            };
            let file = string(s, "file");
            if blank(&s["label"]) || file.is_empty() {
                report("sell needs label, file, first and last".into());
                return false;
            }
            if !out.iter().any(|c| {
                Path::new(file).file_name() == Path::new(string(c, "file")).file_name()
                    && in_range(&json!([a, b]), c)
            }) {
                report(format!(
                    "sell {file} L{a}-{b} is not inside any called function's code"
                ));
                return false;
            }
            true
        })
        .collect();
    (json!(out), json!(sell))
}

pub(super) fn role(ctx: &Spec, role: &Value, report: &mut Report) -> Value {
    let mut out = Value::Object(
        members(role)
            .filter(|(key, _)| {
                [
                    "role",
                    "panels",
                    "compare",
                    "panel_figure",
                    "shows",
                    "truth",
                    "values_note",
                ]
                .contains(&key.as_str())
            })
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect(),
    );
    match string(role, "role") {
        "remake" => {
            if !fields_are_present(role, &["panels", "compare"]) {
                report("run role 'remake' needs panels (the paper's panel names) and compare (what to compare)".into());
            }
            if blank(&role["panel_figure"]) {
                report("run role 'remake' needs panel_figure: the crop of exactly those panels".into());
            } else {
                out["panel_figure"] = ctx.crop(&role["panel_figure"], report);
            }
        }
        "demo" => for key in ["shows", "truth"] {
            if blank(&role[key]) {
                report(format!("run role 'demo' needs {key} (what the synthetic experiment shows; the known truth)"));
            }
        },
        "code" => if blank(&role["shows"]) {
            report("run role 'code' needs shows (what the run shows about the code; it is not a figure of the paper)".into());
        },
        _ => report("run needs role: 'remake' (of named panels), 'code' (for reading the code) or 'demo' (synthetic experiment)".into()),
    }
    out
}

pub(super) fn load(ctx: &Spec, feature: &Value, datasets: &[&Value], report: &mut Report) -> Value {
    let run = string(feature, "kind") == "run";
    need(
        feature,
        if run {
            &["title", "limits"]
        } else {
            &[
                "title",
                "background",
                "question",
                "idea",
                "example",
                "recap",
                "limits",
            ]
        },
        report,
    );
    if !run && !ctx.steps.contains(string(feature, "step")) {
        report("step is not an id of the overview (where we are)".into());
    }
    let dataset = datasets
        .iter()
        .copied()
        .find(|d| d["id"] == feature["dataset"]);
    if dataset.is_none() {
        report("dataset is not an id of datasets (which data the run used)".into());
    }
    let terms = list(&feature["terms"]);
    if terms.is_empty()
        || terms
            .iter()
            .any(|t| !fields_are_present(t, &["term", "meaning"]))
    {
        report("terms: every term needs term and meaning".into());
    }
    if !run {
        let formula = &feature["formula"];
        if formula.is_string() {
            if !na(formula) {
                report("formula must be {text, source} or 'n/a: reason'".into());
            }
        } else if !fields_are_present(formula, &["text", "source"]) {
            report("formula needs text and source".into());
        }
        if !fields_are_present(&feature["selling_point"], &["text", "source"]) {
            report("selling point needs text and source".into());
        }
        if blank(&feature["note_labels"]) {
            report("no note labels (where to read more)".into());
        }
        for key in ["see", "learn", "trouble"] {
            if blank(&feature["reading"][key]) {
                report(format!("reading lacks {key}"));
            }
        }
    }
    let note_links = ctx.links(&feature["note_labels"], report);
    let codes = Value::Object(
        ["author_code", "prior_code"]
            .into_iter()
            .filter_map(|slot| {
                code(ctx, &feature[slot], terms, slot, report).map(|code| (slot.into(), code))
            })
            .collect(),
    );
    let (author, prior) = (&codes["author_code"], &codes["prior_code"]);
    presets(&feature["presets"], author, "preset", report);
    if prior["text"].is_string() {
        presets(
            &feature["prior_presets"],
            prior,
            "earlier-method preset",
            report,
        );
        comparison(feature, author, prior, report);
    }
    let results = results(ctx, feature, author, prior, report);
    run_card(ctx, &feature["run_card"], author, prior, report);
    let (called, sell) = called(ctx, feature, report);
    let figure = ctx.crop(&feature["figure"], report);
    let role = role(ctx, &feature["run_role"], report);
    with(
        feature,
        [
            ("called", called),
            ("sell", sell),
            ("note_links", note_links),
            ("codes", codes),
            ("results", results),
            ("figure", figure),
            (
                "dataset_info",
                dataset.cloned().unwrap_or_else(|| json!({})),
            ),
            ("run_role", role),
        ],
    )
}
