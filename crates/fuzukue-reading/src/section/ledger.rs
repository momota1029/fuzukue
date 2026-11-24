use super::*;

const CONTROLS: &str = r#"<div class="ledctl small"><button onclick="ledAll(this,true)">open all</button> <button onclick="ledAll(this,false)">close all</button></div>"#;

fn concepts(r: &Record) -> &[Value] {
    list(&r.data["concepts"])
}

fn local_concepts<'a>(r: &'a Record, s: &'a Value) -> impl Iterator<Item = &'a Value> {
    list(&s["concepts"])
        .iter()
        .filter_map(|id| concepts(r).iter().find(|c| c["id"] == *id))
}

fn concept_link(c: &Value, local: bool) -> String {
    let prefix = if local { "sc" } else { "a" };
    format!(
        r##"<li><a href="#{prefix}-{id}">{id}</a> <b>{}</b> — {}</li>"##,
        mh(c, "name"),
        mh(c, "gist"),
        id = eh(c, "id")
    )
}

fn concept_card(r: &Record, c: &Value, local: bool, anchor: bool) -> String {
    let id = eh(c, "id");
    let prefix = if local { "sc" } else { "a" };
    let name = mh(c, "name");
    let gist = mh(c, "gist");
    let used = r
        .sections()
        .iter()
        .flat_map(|s| {
            list(&s["claims"])
                .iter()
                .filter(|x| list(&x["concepts"]).contains(&c["id"]))
                .map(move |x| {
                    format!(
                        r##"<a href="#ss-{claim}">{claim}</a> (§{})"##,
                        eh(s, "id"),
                        claim = eh(x, "id")
                    )
                })
        })
        .collect::<Vec<_>>()
        .join(", ");
    let back = if local {
        format!(r##"<a href="#a-{id}">see in the appendix ledger</a>"##)
    } else {
        r.sections()
            .iter()
            .find(|s| list(&s["concepts"]).contains(&c["id"]))
            .map_or_else(String::new, |s| {
                format!(
                    r##"<a href="#sc-{id}">see in the ledger of §{}</a>"##,
                    eh(s, "id")
                )
            })
    };
    let body = format!(
        r#"<p class="gist"><b>In short:</b> {gist}</p><p><b>Explanation:</b> {}</p><p><b>Precise definition:</b> {}</p><p class="small"><b>Claims that use it:</b> {}</p>{}"#,
        mh(c, "explain"),
        mh(c, "definition"),
        fallback(used, "(none)"),
        if back.is_empty() {
            String::new()
        } else {
            format!(r#"<p class="small">↗ {back}</p>"#)
        }
    );
    let anchor = if anchor {
        format!(r#" id="{prefix}-{id}""#)
    } else {
        String::new()
    };
    format!(
        r#"<details class="ledcard"{anchor}><summary><b>{id}</b> {name}<div class="sumgist">{gist}</div></summary><div class="inner">{body}</div></details>"#
    )
}

fn claim_card(r: &Record, s: &Value, c: &Value, local: bool) -> String {
    let id = eh(c, "id");
    let prefix = if local { "ss" } else { "as" };
    let text = mh(c, "text");
    let sec = eh(s, "id");
    let status = fallback(eh(c, "status"), "?");
    let proof = if blank(&c["proof"]) {
        String::new()
    } else {
        format!("<p><b>Proof or locator:</b> {}</p>", mh(c, "proof"))
    };
    let refs = list(&c["concepts"])
        .iter()
        .filter_map(|id| concepts(r).iter().find(|c| c["id"] == *id))
        .map(|c| concept_link(c, local))
        .collect::<String>();
    let refs = if refs.is_empty() {
        refs
    } else {
        format!(r#"<div class="small"><b>Concepts used:</b><ul>{refs}</ul></div>"#)
    };
    let back = if local {
        format!(r##"<a href="#as-{id}">see in the appendix ledger</a>"##)
    } else {
        format!(r##"<a href="#ss-{id}">see in the ledger of §{sec}</a>"##)
    };
    format!(
        r#"<details class="ledcard" id="{prefix}-{id}"><summary><b>{id}</b> <span class="muted small">§{sec} status {status}</span><div class="sumgist">{text}</div></summary><div class="inner"><p><b>Claim:</b> {text}</p>{proof}{refs}<p class="small">↗ {back}</p></div></details>"#
    )
}

pub(super) fn section_html(r: &Record, s: &Value) -> String {
    let concepts: Vec<_> = local_concepts(r, s).collect();
    let claims = list(&s["claims"]);
    if concepts.is_empty() && claims.is_empty() {
        return String::new();
    }
    let mut out = CONTROLS.to_owned();
    if !concepts.is_empty() {
        write!(out, r#"<div class="gistlist"><b>Concepts of this section, one line each:</b><ul>{}</ul></div>"#,
            concepts.iter().map(|c| concept_link(c, true)).collect::<String>()).unwrap();
    }
    for c in concepts {
        let first = r
            .sections()
            .iter()
            .find(|s| list(&s["concepts"]).contains(&c["id"]));
        out.push_str(&concept_card(r, c, true, first == Some(s)));
    }
    for c in claims {
        out.push_str(&claim_card(r, s, c, true));
    }
    out
}

pub(super) fn appendix(r: &Record) -> String {
    let concepts = concepts(r);
    if concepts.is_empty() && !r.sections().iter().any(|s| !blank(&s["claims"])) {
        return String::new();
    }
    let mut out = format!(
        r#"<details class="blk"><summary><b>Concepts</b> ({})</summary><div class="inner">{CONTROLS}"#,
        concepts.len()
    );
    for c in concepts {
        out.push_str(&concept_card(r, c, false, true));
    }
    let claims: Vec<_> = r
        .sections()
        .iter()
        .flat_map(|s| list(&s["claims"]).iter().map(move |c| (s, c)))
        .collect();
    write!(out, r#"</div></details><details class="blk"><summary><b>Claims</b> ({})</summary><div class="inner">{CONTROLS}"#, claims.len()).unwrap();
    for (s, c) in claims {
        out.push_str(&claim_card(r, s, c, false));
    }
    out.push_str("</div></details>");
    out
}

pub(super) fn section_tex(r: &Record, s: &Value) -> String {
    let concepts: Vec<_> = local_concepts(r, s).collect();
    let mut out = String::new();
    if !concepts.is_empty() {
        writeln!(
            out,
            r"{{\small\textbf{{Concepts of this section, one line each}}}}\begin{{itemize}}\small"
        )
        .unwrap();
        for c in &concepts {
            writeln!(out, r"\item {} --- {}", tx(c, "name"), tx(c, "gist")).unwrap();
        }
        writeln!(out, r"\end{{itemize}}").unwrap();
    }
    for c in &concepts {
        writeln!(out, r"\paragraph{{{}\ {}}}\begin{{itemize}}\small\item In short: {}\item Explanation: {}\item Definition: {}\end{{itemize}}",
            tx(c, "id"), tx(c, "name"), tx(c, "gist"), tx(c, "explain"), tx(c, "definition")).unwrap();
    }
    for c in list(&s["claims"]) {
        write!(
            out,
            r"\paragraph{{{}}}\begin{{itemize}}\small\item Claim: {}",
            tx(c, "id"),
            tx(c, "text")
        )
        .unwrap();
        if !blank(&c["proof"]) {
            write!(out, r"\item Proof or locator: {}", tx(c, "proof")).unwrap();
        }
        let refs = list(&c["concepts"])
            .iter()
            .filter_map(|id| self::concepts(r).iter().find(|c| c["id"] == *id))
            .map(|c| format!("{} {} ({})", tx(c, "id"), tx(c, "name"), tx(c, "gist")))
            .collect::<Vec<_>>()
            .join("; ");
        if !refs.is_empty() {
            write!(out, r"\item Concepts used: {refs}").unwrap();
        }
        writeln!(out, r"\end{{itemize}}").unwrap();
    }
    out
}
