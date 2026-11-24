use super::*;

fn level(name: &str) -> Option<(&str, u8)> {
    Some(match name {
        "major" => ("要修正", 0),
        "minor" => ("軽微", 1),
        "expr" => ("表現", 2),
        _ => return None,
    })
}

fn status(name: &str) -> Option<(&str, &str, &str)> {
    Some(match name {
        "fixed" => ("直した", "ok", "green!6"),
        "supplement" => ("補足を載せた", "warn", "orange!8"),
        "open" => ("未解決", "bad", "red!8"),
        "noted" => ("注記のみ", "note", "gray!8"),
        _ => return None,
    })
}

fn risk(name: &str) -> Option<(&str, u8, &str)> {
    Some(match name {
        "high" => ("高", 0, "red!8"),
        "mid" => ("中", 1, "orange!8"),
        "low" => ("低", 2, "yellow!8"),
        _ => return None,
    })
}

pub(super) fn problems(book: &Book) -> Vec<String> {
    let review = &book.review;
    if blank(review) {
        return Vec::new();
    }
    let mut bad = Vec::new();
    let steps: BTreeSet<_> = book
        .blocks
        .iter()
        .flat_map(|b| b.steps.iter().map(|s| s.id.as_str()))
        .collect();
    let mut ids = BTreeSet::new();
    for (field, findings) in [("findings", true), ("risks", false)] {
        for f in list(&review[field]) {
            let id = string(f, "id");
            let lev = string(f, "level");
            if findings {
                if level(lev).is_none() || status(string(f, "status")).is_none() {
                    bad.push(format!("{id}: level/status"));
                }
                for key in ["id", "text", "fix", "where"] {
                    if blank(&f[key]) {
                        bad.push(format!("{id}: missing {key}"));
                    }
                }
            } else if risk(lev).is_none() {
                bad.push(format!("{id}: level"));
            }
            for where_ in list(&f["where"]).iter().map(strval) {
                if STEP_REF
                    .find(where_)
                    .is_ok_and(|m| m.is_some_and(|m| m.as_str() == where_))
                    && !steps.contains(where_)
                {
                    bad.push(format!("{id}: unknown step {where_}"));
                }
            }
            if !ids.insert(id) {
                bad.push(format!("duplicate id {id}"));
            }
            if let Err(error) = support::id(id) {
                bad.push(error.to_string());
            }
        }
    }
    if !list(&review["panel"])
        .iter()
        .any(|p| string(p, "key") == "numeric")
    {
        bad.push("panel has no numeric entry (state that numeric check was not run, as a panel entry with key numeric and covers [])".into());
    }
    bad
}

fn covers(step: &str, scopes: &Value) -> bool {
    list(scopes).iter().map(strval).any(|scope| {
        step == scope
            || step
                .strip_prefix(scope)
                .is_some_and(|rest| rest.starts_with('.'))
    })
}

fn on_block<'a>(list: &'a Value, block: &str) -> Vec<&'a Value> {
    support::list(list)
        .iter()
        .filter(|item| {
            support::list(&item["where"])
                .iter()
                .map(strval)
                .any(|site| site.split('.').next() == Some(block))
        })
        .collect()
}

fn on_step<'a>(list: &'a Value, step: &str) -> Vec<&'a Value> {
    let bid = step.split('.').next().unwrap_or_default();
    support::list(list)
        .iter()
        .filter(|item| {
            support::list(&item["where"])
                .iter()
                .map(strval)
                .any(|site| site == step || site == bid || site.starts_with(&format!("{step}.")))
        })
        .collect()
}

fn roles<'a>(book: &'a Book, step: &str) -> (String, bool, Vec<&'a Value>, Vec<&'a Value>) {
    let mut names = Vec::new();
    let mut numeric = false;
    for p in list(&book.review["panel"])
        .iter()
        .filter(|p| covers(step, &p["covers"]))
    {
        if string(p, "key") == "numeric" {
            numeric = true;
        } else {
            names.push(string(p, "short"));
        }
    }
    if names.is_empty() {
        names.push("なし");
    }
    (
        names.join("・"),
        numeric,
        on_step(&book.review["findings"], step),
        on_step(&book.review["risks"], step),
    )
}

fn counts(book: &Book, block: &str) -> (usize, String, usize) {
    let finds = on_block(&book.review["findings"], block);
    let mut kinds = Vec::<(&str, usize)>::new();
    for f in &finds {
        let name = status(string(f, "status")).map_or("?", |(name, _, _)| name);
        if let Some((_, count)) = kinds.iter_mut().find(|(k, _)| *k == name) {
            *count += 1;
        } else {
            kinds.push((name, 1));
        }
    }
    let columns = kinds
        .into_iter()
        .map(|(name, count)| format!("{name} {count} 件"))
        .collect::<Vec<_>>()
        .join("、");
    let columns = if columns.is_empty() {
        "指摘なし".into()
    } else {
        columns
    };
    (
        finds.len(),
        columns,
        on_block(&book.review["risks"], block).len(),
    )
}

pub(super) fn html_step(book: &Book, step: &str, merged: bool) -> String {
    if blank(&book.review) {
        return String::new();
    }
    let (names, numeric, finds, risks) = roles(book, step);
    let numeric = if numeric {
        "実施（違反なし）"
    } else {
        "未実施（解析的な段）"
    };
    let base = if merged {
        String::new()
    } else {
        book.stem(".html")
    };
    let mut chips = String::new();
    for f in finds {
        let (status, class, _) = status(string(f, "status")).unwrap_or(("?", "note", ""));
        let level = level(string(f, "level")).map_or("?", |(name, _)| name);
        let id = escape(string(f, "id"));
        write!(chips, r#"<a class="fchip st-{class}" href="{base}#find-{id}" title="{}">{id}・{level}・{status}</a>"#, escape(string(f, "text"))).unwrap();
    }
    for r in risks {
        let class = escape(string(r, "level"));
        let name = risk(string(r, "level")).map_or("?", |(name, _, _)| name);
        let id = escape(string(r, "id"));
        write!(chips, r#"<a class="fchip rk-{class}" href="{base}#risk-{id}" title="{}">危うい点 {id}（{name}）</a>"#,
            escape(string(r, "title"))).unwrap();
    }
    if chips.is_empty() {
        chips.push_str("指摘なし");
    }
    format!(
        r#"<div class="rv"><b>査読</b>　検査：{}　／　数値確認：{numeric}　／　指摘：{chips}</div>"#,
        escape(&names)
    )
}

pub(super) fn html_block(book: &Book, id: &str, merged: bool) -> String {
    if blank(&book.review) {
        return String::new();
    }
    let (finds, statuses, risks) = counts(book, id);
    let base = if merged {
        String::new()
    } else {
        book.stem(".html")
    };
    format!(
        r#"<div class="card rv"><p><b>このブロックの査読</b>　指摘 {finds} 件（{statuses}）、危うい点 {risks} 件。{}　<a class="xb" href="{base}#review">査読の結果（全体）</a></p></div>"#,
        html::text(book, strval(&book.review["block"][id]), id, merged)
    )
}

fn sorted(items: &Value, rank: fn(&str) -> u8) -> Vec<&Value> {
    let mut items: Vec<_> = list(items).iter().collect();
    items.sort_by_key(|item| (rank(string(item, "level")), string(item, "id")));
    items
}

fn risks(book: &Book) -> Vec<&Value> {
    sorted(&book.review["risks"], |key| {
        risk(key).map_or(u8::MAX, |(_, rank, _)| rank)
    })
}

fn findings(book: &Book) -> Vec<&Value> {
    sorted(&book.review["findings"], |key| {
        level(key).map_or(u8::MAX, |(_, rank)| rank)
    })
}

fn scopes(panel: &Value) -> String {
    let scopes = list(&panel["covers"])
        .iter()
        .map(strval)
        .collect::<Vec<_>>();
    if scopes.is_empty() {
        "全体".into()
    } else {
        scopes.join("、")
    }
}

fn certainty(f: &Value) -> &str {
    if f["certain"] == false {
        "疑い"
    } else {
        "確定"
    }
}

fn fix_type(f: &Value) -> &str {
    if string(f, "status") == "fixed" {
        "どう直したか"
    } else {
        "補足"
    }
}

pub(super) fn html_front(book: &Book) -> String {
    if blank(&book.review) {
        return String::new();
    }
    let v = &book.review["verdict"];
    let h = |text| html::text(book, text, "", true);
    let field = |obj, key| h(string(obj, key));
    let mut out = format!(
        r#"<section class="blk" id="review"><h2 id="blk-review">査読の結果と、危うい点</h2>
<div class="card key"><p><b>結論（検査した範囲）</b>　{}</p>"#,
        field(v, "summary")
    );
    for item in list(&v["cannot_say"]) {
        write!(out, r#"<p class="lab">{}</p>"#, h(strval(item))).unwrap();
    }
    out.push_str("</div>\n");
    let indicator = &v["indicator"];
    if !blank(indicator) {
        writeln!(out, r#"<div class="card"><p><b>論文になりそうかの指標（上限つき・暫定）</b>　<b>{}</b>　{}</p></div>"#,
            escape(string(indicator, "label")), field(indicator, "why")).unwrap();
    }
    out.push_str("<details open><summary><b>誰がどこまで調べたか</b></summary>\n");
    for p in list(&book.review["panel"]) {
        writeln!(out, r#"<div class="card"><p><b>{}</b>　<span class="lab">{}</span></p><p>{}</p><p class="lab">範囲：{}</p><p><b>結果</b>　{}</p></div>"#,
            escape(string(p, "title")), escape(string(p, "how")), field(p, "what"), escape(&scopes(p)), field(p, "result")).unwrap();
    }
    out.push_str("</details>\n<details open><summary><b>何が危うそうか（重い順）</b></summary>\n");
    for r in risks(book) {
        let id = escape(string(r, "id"));
        let class = escape(string(r, "level"));
        let name = risk(string(r, "level")).map_or("?", |(name, _, _)| name);
        let places = list(&r["where"])
            .iter()
            .map(strval)
            .map(|step| html::step_link(book, step, "", true))
            .collect::<Vec<_>>()
            .join("　");
        writeln!(out, r#"<div class="card rcard rk-{class}" id="risk-{id}"><p><b>{id}　{}</b>　<span class="fchip rk-{class}">危うさ：{name}</span></p><p>{}</p><p><b>これが確かめられれば安心できる</b>　{}</p><p class="lab">該当：{places}</p></div>"#,
            escape(string(r, "title")), field(r, "why"), field(r, "safe_if")).unwrap();
    }
    out.push_str("</details>\n<details open><summary><b>指摘の一覧と、その直し方</b></summary>\n");
    for f in findings(book) {
        let id = escape(string(f, "id"));
        let (status, class, _) = status(string(f, "status")).unwrap_or(("?", "note", ""));
        let level = level(string(f, "level")).map_or("?", |(name, _)| name);
        let places = list(&f["where"])
            .iter()
            .map(strval)
            .map(|step| {
                if step.contains('.') {
                    html::step_link(book, step, "", true)
                } else {
                    escape(step)
                }
            })
            .collect::<Vec<_>>()
            .join("　");
        writeln!(out, r#"<div class="card fcard st-{class}" id="find-{id}"><p><b>{id}</b>　<span class="fchip st-{class}">{status}</span>　<span class="lab">{level}・{}・出所：{}</span></p><p>{}</p><p><b>{}</b>　{}</p><p class="lab">該当：{places}</p></div>"#,
            certainty(f), escape(string(f, "source")), field(f, "text"), fix_type(f), field(f, "fix")).unwrap();
    }
    out.push_str("</details>\n");
    if !blank(&book.review["supplements"]) {
        out.push_str(
            "<details open><summary><b>原稿そのものへの補足（直せなかった点）</b></summary>\n",
        );
        for s in list(&book.review["supplements"]) {
            writeln!(
                out,
                r#"<div class="card"><p><b>{}</b></p><p>{}</p></div>"#,
                escape(string(s, "title")),
                field(s, "text")
            )
            .unwrap();
        }
        out.push_str("</details>\n");
    }
    out.push_str("</section>");
    out
}

pub(super) fn tex_step(book: &Book, step: &str) -> String {
    if blank(&book.review) {
        return String::new();
    }
    let (names, numeric, finds, risks) = roles(book, step);
    let numeric = if numeric {
        "実施（違反なし）"
    } else {
        "未実施（解析的な段）"
    };
    let mut chips = String::new();
    for f in finds {
        let id = string(f, "id");
        let class = status(string(f, "status")).map_or("note", |(_, class, _)| class);
        let color = match class {
            "ok" => "green!50!black",
            "warn" => "orange!80!black",
            "bad" => "red",
            _ => "gray",
        };
        let level = level(string(f, "level")).map_or("?", |(name, _)| name);
        write!(
            chips,
            r"\hyperref[find-{id}]{{\textcolor{{{color}}}{{[{id}・{level}]}}}}~"
        )
        .unwrap();
    }
    for r in risks {
        let id = string(r, "id");
        write!(
            chips,
            r"\hyperref[risk-{id}]{{\textcolor{{red!70!black}}{{[危うい点~{id}]}}}}~"
        )
        .unwrap();
    }
    if chips.is_empty() {
        chips.push_str("なし");
    }
    format!(
        r"\textbf{{\textcolor{{blue}}{{査読}}}}\quad 検査：{}\quad 数値確認：\textbf{{{numeric}}}\quad 指摘：{chips}\par",
        tex::text(&names)
    )
}

pub(super) fn tex_block(book: &Book, id: &str) -> String {
    if blank(&book.review) {
        return String::new();
    }
    let (finds, statuses, risks) = counts(book, id);
    format!(
        r"\begin{{tcolorbox}}[colback=green!3,colframe=green!30!black!40,boxrule=.4pt,left=5pt,right=5pt,top=2pt,bottom=2pt,fontupper=\small]\textbf{{このブロックの査読}}\quad 指摘 {finds} 件（{statuses}）、危うい点 {risks} 件。{}\quad\hyperref[review]{{査読の結果（全体）}}\end{{tcolorbox}}",
        tex::text(strval(&book.review["block"][id]))
    )
}

fn where_tex(value: &Value, link_blocks: bool) -> String {
    list(value)
        .iter()
        .map(strval)
        .map(|loc| {
            if loc.contains('.') || link_blocks {
                format!(r"\hyperref[{loc}]{{{loc}}}")
            } else {
                tex::text(loc)
            }
        })
        .collect::<Vec<_>>()
        .join(r"\quad ")
}

pub(super) fn tex_front(book: &Book) -> String {
    if blank(&book.review) {
        return String::new();
    }
    let v = &book.review["verdict"];
    let field = |obj, key| tex::text(string(obj, key));
    let mut out = r"\clearpage\phantomsection\hypertarget{review}{}\label{review}\section*{査読の結果と、危うい点}\addcontentsline{toc}{section}{査読の結果と、危うい点}".to_owned();
    out.push('\n');
    let bounds: String = list(&v["cannot_say"])
        .iter()
        .map(|x| format!(r"{{\small {}}}\par ", tex::text(strval(x))))
        .collect();
    writeln!(out, r"\begin{{tcolorbox}}[colback=blue!4,colframe=blue!30,boxrule=.4pt,breakable,left=5pt,right=5pt,top=2pt,bottom=2pt]\textbf{{結論（検査した範囲）}}\quad {}\par {bounds}\end{{tcolorbox}}", field(v, "summary")).unwrap();
    let indicator = &v["indicator"];
    if !blank(indicator) {
        writeln!(
            out,
            r"\textbf{{論文になりそうかの指標（上限つき・暫定）}}\quad \textbf{{{}}}\quad {}\par",
            field(indicator, "label"),
            field(indicator, "why")
        )
        .unwrap();
    }
    writeln!(
        out,
        r"\paragraph{{\textcolor{{blue}}{{誰がどこまで調べたか}}}}"
    )
    .unwrap();
    for p in list(&book.review["panel"]) {
        writeln!(out, r"\begin{{tcolorbox}}[colback=white,colframe=gray!40,boxrule=.4pt,breakable,left=5pt,right=5pt,top=2pt,bottom=2pt,fontupper=\small]\textbf{{{}}}\quad{{\footnotesize {}}}\par {}\par\textbf{{範囲}}：{}\par\textbf{{結果}}\quad {}\end{{tcolorbox}}",
            field(p, "title"), field(p, "how"), field(p, "what"), tex::text(&scopes(p)), field(p, "result")).unwrap();
    }
    writeln!(
        out,
        r"\paragraph{{\textcolor{{blue}}{{何が危うそうか（重い順）}}}}"
    )
    .unwrap();
    for r in risks(book) {
        let id = string(r, "id");
        let (name, _, color) = risk(string(r, "level")).unwrap_or(("?", u8::MAX, "white"));
        writeln!(out, r"\phantomsection\hypertarget{{risk-{id}}}{{}}\label{{risk-{id}}}\begin{{tcolorbox}}[colback={color},colframe=red!30,boxrule=.4pt,breakable,left=5pt,right=5pt,top=2pt,bottom=2pt,fontupper=\small]\textbf{{{id}\quad {}}}\quad（危うさ：{name}）\par {}\par\textbf{{これが確かめられれば安心できる}}\quad {}\par{{\footnotesize 該当：{}}}\end{{tcolorbox}}",
            field(r, "title"), field(r, "why"), field(r, "safe_if"), where_tex(&r["where"], true)).unwrap();
    }
    writeln!(
        out,
        r"\paragraph{{\textcolor{{blue}}{{指摘の一覧と、その直し方}}}}"
    )
    .unwrap();
    for f in findings(book) {
        let id = string(f, "id");
        let (name, _, color) = status(string(f, "status")).unwrap_or(("?", "note", "white"));
        let level = level(string(f, "level")).map_or("?", |(name, _)| name);
        writeln!(out, r"\phantomsection\hypertarget{{find-{id}}}{{}}\label{{find-{id}}}\begin{{tcolorbox}}[colback={color},colframe=gray!45,boxrule=.4pt,breakable,left=5pt,right=5pt,top=2pt,bottom=2pt,fontupper=\small]\textbf{{{id}}}\quad[{name}・{level}・{}]\quad{{\footnotesize 出所：{}}}\par {}\par\textbf{{{}}}\quad {}\par{{\footnotesize 該当：{}}}\end{{tcolorbox}}",
            certainty(f), field(f, "source"), field(f, "text"), fix_type(f), field(f, "fix"), where_tex(&f["where"], false)).unwrap();
    }
    if !blank(&book.review["supplements"]) {
        writeln!(
            out,
            r"\paragraph{{\textcolor{{blue}}{{原稿そのものへの補足（直せなかった点）}}}}"
        )
        .unwrap();
        for s in list(&book.review["supplements"]) {
            writeln!(out, r"\begin{{tcolorbox}}[colback=orange!5,colframe=orange!35,boxrule=.4pt,breakable,left=5pt,right=5pt,top=2pt,bottom=2pt,fontupper=\small]\textbf{{{}}}\par {}\end{{tcolorbox}}", field(s, "title"), field(s, "text")).unwrap();
        }
    }
    out
}
