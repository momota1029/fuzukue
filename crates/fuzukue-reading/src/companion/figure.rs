use super::*;

fn location(ctx: &Spec, text: &str, report: &mut Report) {
    static WHERE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"([\w./-]+\.\w+)\s+L(\d+)(?:[–-](\d+))?").unwrap());
    for captures in WHERE.captures_iter(text) {
        let (file, a) = (&captures[1], &captures[2]);
        let b = captures.get(3).map_or(a, |m| m.as_str());
        let name = Path::new(file).file_name();
        let found = ctx.methods.values().find_map(|method| {
            let dir = ctx.base.join(string(method, "root"));
            walkdir::WalkDir::new(dir)
                .into_iter()
                .filter_map(Result::ok)
                .find(|entry| entry.file_type().is_file() && Some(entry.file_name()) == name)
        });
        let Some(entry) = found else {
            report(format!("where: {file} not found under any method root"));
            continue;
        };
        if let Ok(text) = fs::read(entry.path()) {
            let lines = String::from_utf8_lossy(&text).lines().count();
            if b.parse::<usize>().is_ok_and(|last| last > lines) {
                report(format!(
                    "where: {file} L{a}-{b} is beyond the file ({lines} lines)"
                ));
            }
        }
    }
}

fn reads(figure: &Value, features: &BTreeMap<&str, &Value>, report: &mut Report) {
    let reads = list(&figure["reads"]);
    if reads.is_empty()
        || reads
            .iter()
            .any(|r| !fields_are_present(r, &["mark", "see", "learn"]))
    {
        report(
            "reads: every mark needs mark, see and learn (the paper figure is always explained)"
                .into(),
        );
    }
    for reading in reads {
        let mark = string(reading, "mark");
        let check = &reading["check"];
        if blank(check) {
            report(format!("reads mark {mark}: needs a check (which preset to press and what to look at, or paper_only with the reason)"));
        } else if truth(&check["paper_only"]) {
            if blank(&check["reason"]) {
                report(format!("reads mark {mark}: paper_only needs the reason"));
            }
        } else {
            let step = string(check, "step");
            let preset = string(check, "preset");
            if let Some(f) = features.get(step) {
                if preset.is_empty()
                    || !list(&f["presets"])
                        .iter()
                        .chain(list(&f["prior_presets"]))
                        .any(|p| string(p, "label").starts_with(preset))
                {
                    report(format!(
                        "reads mark {mark}: preset '{preset}' is not a preset of {step}"
                    ));
                }
            } else {
                report(format!(
                    "reads mark {mark}: check.step must be a step unit or demo id"
                ));
            }
            for key in ["look", "result"] {
                if blank(&check[key]) {
                    report(format!(
                        "reads mark {mark}: check lacks {key} (what to look at; what was observed)"
                    ));
                }
            }
        }
    }
}

fn demo(figure: &Value, features: &BTreeMap<&str, &Value>, level: &str, report: &mut Report) {
    if blank(&figure["demo"]) {
        if level != "L4" {
            report("no demo: a figure with code needs a synthetic experiment to check the claims by hand (or level L4)".into());
        }
        return;
    }
    let Some(demo) = features
        .get(string(&figure["demo"], "step"))
        .filter(|f| string(&f["run_role"], "role") == "demo")
    else {
        report("demo.step must be a run unit whose run_role is 'demo'".into());
        return;
    };
    if blank(&demo["codes"]["author_code"]["params"]) {
        report("the demo is the one run with controls: it needs at least one control".into());
    }
    if blank(&demo["called"]) {
        report("the demo needs called: the whole code of the functions whose arguments are its controls (folded in the run), with sell ranges highlighted".into());
    } else if blank(&demo["sell"]) {
        report("the demo needs sell: the ranges of the called code that are the selling points of the figure".into());
    }
    if blank(&demo["run_role"]["values_note"]) {
        report("demo run_role needs values_note (how the call differs from the call behind the paper's figure, and why)".into());
    }
    if features
        .get(string(&figure["reproduction"], "step"))
        .is_some_and(|run| {
            string(run, "kind") == "run" && !blank(&run["codes"]["author_code"]["params"])
        })
    {
        report("a remake next to the demo must have no controls: one run with controls per figure (controls only on the demo)".into());
    }
}

fn reproduction(
    ctx: &Spec,
    rep: &Value,
    features: &BTreeMap<&str, &Value>,
    report: &mut Report,
) -> Value {
    let level = string(rep, "level");
    if !LEVELS.contains(&level) {
        report("reproduction level must be L1–L4".into());
        return rep.clone();
    }
    if level == "L4" {
        if blank(&rep["reason"]) {
            report(
                "L4 needs the reason (no code, or no runnable data yet) and what it links to"
                    .into(),
            );
        }
        return rep.clone();
    }
    let role = feature::role(ctx, rep, report);
    let mut data = with(
        rep,
        members(&role).filter_map(|(key, value)| {
            [
                "role",
                "panels",
                "compare",
                "panel_figure",
                "shows",
                "truth",
                "values_note",
            ]
            .into_iter()
            .find(|k| *k == key)
            .map(|k| (k, value.clone()))
        }),
    );
    let step = features.get(string(rep, "step"));
    if step.is_none() {
        report("reproduction step must be a step unit id (the run shown)".into());
    }
    if let Some(extra) = data.get_mut("extra_steps").and_then(Value::as_array_mut) {
        for run in extra {
            let step = string(run, "step");
            if !features.contains_key(step) {
                report(format!("extra run '{step}' is not a step unit id"));
            } else {
                let role = feature::role(ctx, run, report);
                if let Some(values) = run.as_object_mut() {
                    values.extend(members(&role).map(|(key, value)| (key.clone(), value.clone())));
                }
            }
        }
    }
    for key in ["corresponds", "not_transfer"] {
        if blank(&rep[key]) {
            report(format!("reproduction lacks {key} (which panel and claim the run corresponds to; what does not transfer)"));
        }
    }
    if let Some(step) = step {
        let data_level = string(&step["dataset_info"], "level");
        if !data_level.is_empty() && data_level != level {
            report(format!(
                "reproduction level {level} differs from the level of the data used ({data_level})"
            ));
        }
    }
    data
}

fn points(points: &Value, level: &str, features: &BTreeMap<&str, &Value>, report: &mut Report) {
    if blank(points) && level != "L4" {
        report("no figure selling point ↔ code selling point".into());
    }
    for (i, point) in list(points).iter().enumerate() {
        let mut report = |msg| report(format!("point {}: {msg}", i + 1));
        need(point, &["panel", "figure_point", "code_point"], &mut report);
        let step = &point["step"];
        if let Some(f) = features.get(strval(step)) {
            for (side, slot) in [("author", "author_code"), ("prior", "prior_code")] {
                let focus = &point["focus"][side];
                if !focus.is_null() && !in_range(focus, &f["codes"][slot]) {
                    report(format!(
                        "focus {side} {focus} outside the step unit's excerpt"
                    ));
                }
            }
        } else if !na(step) {
            report("step must be a step unit id or 'n/a: reason'".into());
        }
    }
}

fn figure(ctx: &Spec, g: &Value, features: &BTreeMap<&str, &Value>, report: &mut Report) -> Value {
    let level = string(&g["reproduction"], "level");
    if truth(&g["pending"]) {
        need(g, &["id", "label", "title"], report);
        if !LEVELS.contains(&level) {
            report(
                "pending figure: give the reproduction level the readiness review (R32) found"
                    .into(),
            );
        }
        return with(g, [("note_links", ctx.links(&g["note_labels"], report))]);
    }
    need(
        g,
        &["id", "label", "title", "plain", "trouble", "handoff"],
        report,
    );
    if !fields_are_present(&g["claim"], &["text", "source"]) {
        report("claim needs the main-text sentence or caption statement and its locator".into());
    }
    reads(g, features, report);
    demo(g, features, level, report);
    if level != "L4" {
        let differences = list(&g["difference"]);
        if differences.is_empty() {
            report("no difference: say which steps and lines make the difference the figure shows (to the earlier method or between conditions)".into());
        }
        for (i, diff) in differences.iter().enumerate() {
            let mut report = |msg| report(format!("difference {}: {msg}", i + 1));
            if !fields_are_present(diff, &["text", "where"]) {
                report("needs text and where (file and lines)".into());
            }
            let steps = list(&diff["steps"]);
            if steps.is_empty() || steps.iter().any(|step| !ctx.steps.contains(strval(step))) {
                report("steps must be ids of the whole-method map".into());
            }
            if !matches!(string(diff, "by"), "paper" | "note" | "speaker") {
                report("by must be 'paper', 'note' or 'speaker' (whose statement it is)".into());
            }
            location(ctx, string(diff, "where"), &mut report);
        }
    }
    let talk = &g["talk"];
    if !talk.is_null() {
        for key in ["takeaway", "picture", "property"] {
            if blank(&talk[key]) {
                report(format!("talk needs {key} (one sentence to take away; the picture to show; the one-word property of the method this figure shows)"));
            }
        }
        if blank(&talk["panels"]["show"]) {
            report("talk.panels.show: the panels to show (the rest, panels.rest, are said in a sentence or left to the appendix; the figure itself is never skipped)".into());
        }
    }
    let presenter = &g["presenter"];
    for key in ["say", "point", "not_shown"] {
        if blank(&presenter[key]) {
            report(format!("presenter notes lack {key}"));
        }
    }
    for (i, question) in list(&presenter["questions"]).iter().enumerate() {
        if !fields_are_present(question, &["q", "a"]) {
            report(format!("presenter question {}: needs q and a", i + 1));
        }
    }
    let rep = reproduction(ctx, &g["reproduction"], features, report);
    points(&g["points"], level, features, report);
    let mut data = g.clone();
    if let Some(supplements) = data.get_mut("supplementary").and_then(Value::as_array_mut) {
        for supplement in supplements {
            if !fields_are_present(supplement, &["label", "cited_at", "why"]) {
                report("supplementary figure needs label, cited_at (main-text locator) and why it is needed here".into());
            }
            if truth(&supplement["figure"]) {
                supplement["figure"] = ctx.crop(&supplement["figure"], report);
            }
        }
    }
    if blank(&g["note_labels"]) {
        report("no note labels (where to read more)".into());
    }
    let links = ctx.links(&g["note_labels"], report);
    let crop = ctx.crop(&g["figure"], report);
    data = with(&data, [("note_links", links), ("figure", crop)]);
    if LEVELS.contains(&level) && level != "L4" {
        data["reproduction"] = rep;
    }
    data
}

pub(super) fn load(
    ctx: &Spec,
    spec: &Value,
    features: &[Value],
    problems: &mut Vec<String>,
) -> Vec<Value> {
    let features: BTreeMap<_, _> = features
        .iter()
        .map(|item| (string(item, "id"), item))
        .collect();
    let figures = list(&spec["figures"]);
    let nums: Vec<_> = figures.iter().map(|g| g["number"].as_u64()).collect();
    match spec["paper"]["main_figures"].as_u64() {
        Some(total) if total > 0 => {
            if nums.len() as u64 != total
                || nums
                    .iter()
                    .enumerate()
                    .any(|(i, n)| *n != Some(i as u64 + 1))
            {
                let numbers = figures
                    .iter()
                    .map(|g| g["number"].to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                problems.push(format!("figures: main-text figures must be listed once each, in order 1..{total} (found [{numbers}])"));
            }
        }
        _ => problems
            .push("paper: main_figures (the number of main-text figures) is required".into()),
    }
    let mut ids = BTreeSet::new();
    let mut used = BTreeSet::new();
    let output = figures
        .iter()
        .map(|g| {
            let id = default(g, "id", "?");
            if !ids.insert(id) {
                problems.push(format!("figure {id}: duplicate id"));
            }
            for step in [
                string(&g["demo"], "step"),
                string(&g["reproduction"], "step"),
            ]
            .into_iter()
            .chain(list(&g["points"]).iter().map(|p| string(p, "step")))
            .chain(
                list(&g["reproduction"]["extra_steps"])
                    .iter()
                    .map(|p| string(p, "step")),
            ) {
                used.insert(step);
            }
            let label = default(g, "label", id);
            figure(ctx, g, &features, &mut |msg| {
                problems.push(format!("figure {label}: {msg}"))
            })
        })
        .collect();
    for id in features.keys() {
        if !used.contains(id) {
            problems.push(format!(
                "step unit {id}: not linked from any figure (tie each step to the main text)"
            ));
        }
    }
    output
}
