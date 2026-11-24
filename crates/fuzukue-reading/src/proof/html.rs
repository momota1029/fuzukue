use super::*;

const TEMPLATE: &str = include_str!("../assets/review-page-template.html");
const CSS: &str = include_str!("../assets/proof.css");
const NAV: &str = r##"<script>(function(){
const nav=document.getElementById('nav');
const box=document.createElement('div');box.className='ctl2';
box.innerHTML='<b>説明の文章</b><button id="narr">隠す / 表示</button><b>文脈の欄</b><button id="ctxb">隠す / 表示</button>';
nav.appendChild(box);
document.getElementById('ctxb').onclick=()=>document.body.classList.toggle('hide-ctx');
document.getElementById('narr').onclick=()=>document.body.classList.toggle('hide-narr');
function openTo(id){let t=document.getElementById(id);if(!t)return;
for(let p=t;p;p=p.parentElement){if(p.tagName==='DETAILS')p.open=true;}
t.scrollIntoView({block:'center'});t.classList.add('flash');setTimeout(()=>t.classList.remove('flash'),2500);}
document.addEventListener('click',function(e){const a=e.target.closest&&e.target.closest('a[href^="#"]');
if(a){e.preventDefault();const id=decodeURIComponent(a.getAttribute('href').slice(1));
history.replaceState(null,'','#'+encodeURIComponent(id));openTo(id);}});
if(location.hash)openTo(decodeURIComponent(location.hash.slice(1)));
})();</script>"##;

pub(super) fn step_link(book: &Book, step: &str, block: &str, merged: bool) -> String {
    let bid = step.split('.').next().unwrap_or_default();
    let base = if merged || bid == block {
        String::new()
    } else {
        book.block_html(bid)
    };
    let loc = if step.contains('.') {
        step.to_owned()
    } else {
        format!("blk-{step}")
    };
    format!(
        r#"<a class="xb" href="{base}#{}">{}</a>"#,
        escape(&loc),
        escape(step)
    )
}

pub(super) fn text(book: &Book, text: &str, block: &str, merged: bool) -> String {
    let mut out = String::new();
    for (i, segment) in text.split('$').enumerate() {
        if i % 2 == 1 {
            write!(out, r#"<span class="m">{}</span>"#, escape(segment)).unwrap();
            continue;
        }
        let escaped = escape(segment);
        let lines = LINE_REF.replace_all(escaped.as_str(), |m: &fancy_regex::Captures<'_, str>| {
            let id = m.get(1).or_else(|| m.get(2)).unwrap().as_str();
            let number = book.numbers.get(id).map_or(id, String::as_str);
            format!(r##"<a href="#{id}">({number})</a>"##)
        });
        let steps = STEP_REF.replace_all(lines.as_ref(), |m: &fancy_regex::Captures<'_, str>| {
            step_link(book, &m[1], block, merged)
        });
        let blocks = BLOCK_REF.replace_all(steps.as_ref(), |m: &fancy_regex::Captures<'_, str>| {
            let b = &m[1];
            let base = if merged {
                String::new()
            } else {
                book.block_html(b)
            };
            let id = match m.get(2) {
                Some(n) => format!("{b}.{}", n.as_str()),
                None => format!("blk-{b}"),
            };
            format!(r#"<a class="xb" href="{base}#{id}">{}</a>"#, &m[0])
        });
        out.push_str(&blocks);
    }
    out
}

fn page(body: &str, title: &str, css: &str, js: &str, nav: bool) -> String {
    let common = TEMPLATE.replace("svg{width:100%;height:auto;background:#fff;border:1px solid var(--line);border-radius:8px}", "")
        .replace("{{LANG}}", "ja").replace("{{L:toc}}", "目次").replace("{{L:display}}", "表示")
        .replace("{{L:btn_all}}", "すべて開く").replace("{{L:btn_none}}", "すべて閉じる")
        .replace("{{L:btn_key}}", "キーだけ開く（既定）")
        .replace("{{EXTRA_SCRIPT}}", if nav { NAV } else { "" });
    common
        .replace("/*KATEX_CSS*/", css)
        .replace("/*KATEX_JS*/", js)
        .replace("{{TITLE}}", &escape(title))
        .replace("{{BODY}}", &format!("<style>{CSS}</style>\n{body}"))
}

fn page_number(aux: &Aux, id: &str) -> Option<u32> {
    aux.get(id).map(|d| d.page)
}

fn pdf_link(book: &Book, aux: &Aux, id: &str, label: &str, class: &str) -> String {
    let mut href = book.stem(".pdf");
    if let Some(page) = page_number(aux, id) {
        write!(href, "#page={page}").unwrap();
    }
    format!(r#"<a class="{class}" href="{href}">{}</a>"#, escape(label))
}

fn diff_chips(book: &Book, ids: &Value, merged: bool) -> String {
    let base = if merged {
        String::new()
    } else {
        book.stem(".html")
    };
    list(ids)
        .iter()
        .map(|n| {
            let d = book.diff(n);
            format!(
                r#"<a class="dchip" href="{base}#diff-{n}" title="{}">難所 {n}</a>"#,
                escape(string(d, "title"))
            )
        })
        .collect()
}

fn step_context(book: &Book, block: &Block, step: &Step, merged: bool) -> String {
    let d = book.step_context(&step.id);
    let mut out = String::new();
    if !blank(&d["why"]) {
        write!(
            out,
            "<div><b>なぜここで要るか</b>　{} {}</div>",
            text(book, string(d, "why"), &block.id, merged),
            diff_chips(book, &d["diffs"], merged)
        )
        .unwrap();
    }
    for (caption, links) in [("使われる先", &book.used_by), ("前提にする段", &book.uses)]
    {
        if let Some(ids) = links.get(&step.id) {
            let links = ids
                .iter()
                .map(|id| step_link(book, id, &block.id, merged))
                .collect::<Vec<_>>()
                .join("　");
            write!(out, "<div><b>{caption}</b>　{links}</div>").unwrap();
        }
    }
    if let Some(class) = book.classification(step) {
        write!(
            out,
            r#"<div class="lab"><b>分類</b>　台帳 {}：{}</div>"#,
            escape(&step.ledger),
            escape(&class)
        )
        .unwrap();
    }
    out.push_str(&review::html_step(book, &step.id, merged));
    if out.is_empty() {
        out
    } else {
        format!(r#"<div class="ctx">{out}</div>"#)
    }
}

fn block_context(book: &Book, block: &Block, merged: bool) -> String {
    let data = book.block_context(&block.id);
    let mut out = String::new();
    if !blank(&data["role"]) {
        write!(
            out,
            r#"<div class="card ctxb"><p><b>このブロックの役割</b>　{}　{}</p></div>"#,
            text(book, string(data, "role"), &block.id, merged),
            diff_chips(book, &data["diffs"], merged)
        )
        .unwrap();
    }
    out.push_str(&review::html_block(book, &block.id, merged));
    out
}

fn prose(book: &Book, text_: &str, block: &str, narration: bool, merged: bool) -> String {
    let class = if narration { "narr" } else { "prose" };
    format!(
        r#"<p class="{class}">{}</p>"#,
        text(book, text_, block, merged)
    )
}

fn block_html(book: &Book, block: &Block, aux: &Aux, merged: bool) -> String {
    let mut out = String::new();
    let bid = &block.id;
    let h = |s| text(book, s, bid, merged);
    let heading = if merged { "h2" } else { "h1" };
    if !merged {
        writeln!(out, r#"<p class="lab"><a href="{}.html#blk-{bid}">↩ 全体版（1 ページ）へ</a>　<a href="{}-index.html">↩ 索引へ</a></p>"#,
            book.prefix, book.prefix).unwrap();
    }
    writeln!(
        out,
        r#"<{heading} id="blk-{bid}">{bid}　{}</{heading}>"#,
        h(&block.title)
    )
    .unwrap();
    writeln!(out, r#"<p class="sub">{}<br><span class="lab">完全版の PDF：{}　同じフォルダに置いて使う。各式の「PDF ↗」は、PDF の同じ式のページへ移る。</span></p>"#,
        h(&block.loc), pdf_link(book, aux, bid, "PDF を開く", "pdfl")).unwrap();
    writeln!(out, "{}", block_context(book, block, merged)).unwrap();
    writeln!(
        out,
        r#"<div class="card key"><p><b>主張</b>　{}</p></div>"#,
        h(&block.claim)
    )
    .unwrap();
    for (line, narration) in &block.intro {
        writeln!(out, "{}", prose(book, line, bid, *narration, merged)).unwrap();
    }
    for step in &block.steps {
        let id = &step.id;
        let title = h(&step.title);
        writeln!(out, r#"<details class="step t-keyc" id="{id}" open><summary><span class="tag t-key">{}</span><b>{id}</b>　{title}</summary>"#,
            escape(&step.ledger)).unwrap();
        writeln!(out, "{}", step_context(book, block, step, merged)).unwrap();
        let page = page_number(aux, id).map_or_else(|| "?".into(), |p| p.to_string());
        writeln!(
            out,
            r#"<div class="meta">原稿：{}　/　{}</div>"#,
            h(&step.loc),
            pdf_link(book, aux, id, &format!("↗ PDF の {id}（p.{page}）"), "pdfl")
        )
        .unwrap();
        writeln!(
            out,
            r#"<div class="l1"><b>主張</b>　{}</div>"#,
            h(&step.claim)
        )
        .unwrap();

        let mut lhs: Option<String> = None;
        let mut items = step.items.iter().peekable();
        while let Some(item) = items.next() {
            let line = match item {
                Item::Prose { text, narration } => {
                    writeln!(out, "{}", prose(book, text, bid, *narration, merged)).unwrap();
                    lhs = None;
                    continue;
                }
                Item::Formula(line) => line,
            };
            let id = &line.id;
            if let Some(topic) = &line.fine {
                let mut count = 1;
                while items
                    .peek()
                    .is_some_and(|i| i.formula().is_some_and(|line| line.fine.is_some()))
                {
                    items.next();
                    count += 1;
                }
                let topic = if topic.is_empty() { "細部" } else { topic };
                let page = page_number(aux, id).map_or_else(|| "?".into(), |p| p.to_string());
                writeln!(out, r#"<div class="fine" id="{id}">▸ ここに <b>{count} 行</b>（{}）が入る。長いので PDF だけに書いてある：{}</div>"#,
                    escape(topic), pdf_link(book, aux, id, &format!("PDF の該当の式（p.{page}）"), "pdfl")).unwrap();
                lhs = None;
                continue;
            }
            let tex = line.tex.replace(r"\newrow", r"\;");
            let formula = if let Some(lhs) = lhs.as_deref().filter(|_| continuation(&tex)) {
                format!(r"\begin{{aligned}}\phantom{{{lhs}}}&{tex}\end{{aligned}}")
            } else if let Some(cut) = relations(&tex).first().copied() {
                lhs = Some(tex[..cut].into());
                format!(
                    r"\begin{{aligned}}{}&{}\end{{aligned}}",
                    &tex[..cut],
                    &tex[cut..]
                )
            } else {
                lhs = None;
                tex
            };
            let number = book.numbers.get(id).map_or_else(String::new, |n| {
                format!(r#"<span class="eqno">({n})</span>"#)
            });
            let annotation = if line.reconstructed {
                r#"<span class="tg" title="原稿に書かれておらず、読解版で補った行">†補足</span>"#
            } else {
                ""
            };
            let lead = if line.lead.is_empty() {
                String::new()
            } else {
                format!(r#"<div class="lead">{}{annotation}</div>"#, h(&line.lead))
            };
            let link = pdf_link(book, aux, id, "PDF ↗", "pdfi");
            writeln!(out, r#"<div class="eq" id="{id}">{lead}<div class="fm"><div class="d">{}</div>{number}{link}</div></div>"#,
                escape(&formula)).unwrap();
        }
        out.push_str("</details>\n");
    }
    out
}

fn context(book: &Book) -> String {
    let mut out =
        r#"<section class="blk" id="problem"><h2 id="blk-problem">この問題と、どこが難しいか</h2>"#
            .to_owned();
    let h = |s| text(book, s, "", true);
    for (head, key) in [
        ("問題設定", "setting"),
        ("何を示すか", "goal"),
        ("難所（素直にやると何が起きるか）", "diffs"),
        ("戦略", "strategy"),
    ] {
        let mut body = String::new();
        if key == "diffs" {
            for n in book.difficulty_ids() {
                let d = book.diff(&json!(n));
                let blocks = book
                    .blocks
                    .iter()
                    .filter(|b| list(&book.block_context(&b.id)["diffs"]).contains(&json!(n)))
                    .map(|b| step_link(book, &b.id, "", true))
                    .collect::<Vec<_>>()
                    .join("　");
                write!(body, r#"<div class="card" id="diff-{n}"><p><b>難所 {n}：{}</b></p><p>{}</p><p class="lab">受け持つブロック：{blocks}</p></div>"#,
                    escape(string(d, "title")), h(string(d, "text"))).unwrap();
            }
        } else {
            for text in list(&book.context["page"][key]) {
                write!(body, r#"<p class="prose">{}</p>"#, h(strval(text))).unwrap();
            }
        }
        if !body.is_empty() {
            writeln!(
                out,
                "<details open><summary><b>{head}</b></summary>{body}</details>"
            )
            .unwrap();
        }
    }
    out.push_str("</section>\n");
    out
}

fn full(book: &Book, aux: &Aux) -> String {
    let title = escape(&book.title);
    let mut out = format!(
        r#"<style>.blk{{margin:2.5em 0;padding-top:1em;border-top:3px solid #c9d6ee}}.blkbar a{{margin-right:.6em}}</style>
<h1 id="top">{title}</h1><p class="sub">全ブロックを 1 ページに並べた版。左の目次、または下の一覧から各ブロックへ移れる。各式の「PDF ↗」は、そのブロックの PDF の同じ式へ移る（PDF は同じフォルダに置いて使う）。他のブロックを引く箇所は、このページの中で移る。</p>"#
    );
    let mut chips = vec![r##"<a class="pdfl" href="#blk-problem">この問題と難所</a>"##.to_owned()];
    if !blank(&book.review) {
        chips.push(r##"<a class="pdfl rvl" href="#blk-review">査読の結果と危うい点</a>"##.into());
    }
    for b in &book.blocks {
        write_block_link(&mut chips, b);
    }
    let order = book
        .blocks
        .iter()
        .map(|b| b.id.as_str())
        .collect::<Vec<_>>()
        .join(" → ");
    writeln!(out, r#"<div class="card"><p><b>ブロック</b>　{}</p><p class="lab">読む順序は {order}。</p></div>"#, chips.join("　")).unwrap();
    if !blank(&book.review) {
        writeln!(out, r##"<div class="card rv"><p><b>査読の要点</b>　{}　<a class="xb" href="#blk-review">詳しくは「査読の結果と危うい点」へ</a></p></div>"##,
            text(book, string(&book.review["verdict"], "summary"), "", true)).unwrap();
    }
    out.push_str(&context(book));
    out.push_str(&review::html_front(book));
    for (i, block) in book.blocks.iter().enumerate() {
        let prev = i
            .checked_sub(1)
            .and_then(|i| book.blocks.get(i))
            .map_or_else(String::new, |b| {
                format!(r##"<a href="#blk-{0}">← {0}</a>"##, b.id)
            });
        let next = book.blocks.get(i + 1).map_or_else(String::new, |b| {
            format!(r##"<a href="#blk-{0}">{0} →</a>"##, b.id)
        });
        let bar = format!(
            r##"<p class="lab blkbar">{prev}　<a href="#top">↑ 先頭へ</a>　<a href="{}-index.html">索引</a>　{next}</p>"##,
            book.prefix
        );
        writeln!(
            out,
            r#"<section class="blk">{bar}{}{bar}</section>"#,
            block_html(book, block, aux, true)
        )
        .unwrap();
    }
    out
}

fn write_block_link(chips: &mut Vec<String>, b: &Block) {
    chips.push(format!(
        r##"<a class="pdfl" href="#blk-{}">{} {}</a>"##,
        b.id,
        b.id,
        escape(&b.title)
    ));
}

fn index(book: &Book, aux: &Aux) -> String {
    let title = escape(&book.title);
    let prefix = &book.prefix;
    let mut out = format!(
        r#"<h1>{title}：ブロックの索引</h1><p class="sub">各ブロックは、1 つの HTML（主要な道筋）と、全体で 1 つの PDF（式変形をすべて書いた完全版）にある。全ブロックを 1 ページにした版は <a class="pdfl" href="{prefix}.html">{prefix}.html</a>（先頭に <a class="pdfl" href="{prefix}.html#blk-problem">この問題と難所</a> の節がある）。</p>"#
    );
    for b in &book.blocks {
        let id = &b.id;
        let h = |s| text(book, s, id, false);
        let file = book.block_html(id);
        let lines: Vec<_> = b
            .steps
            .iter()
            .flat_map(|s| s.items.iter().filter_map(Item::formula))
            .collect();
        write!(out, r#"<div class="card"><h2>{id}　{}</h2>"#, h(&b.title)).unwrap();
        let role = h(string(book.block_context(id), "role"));
        if !role.is_empty() {
            write!(out, r#"<p class="lab">{role}</p>"#).unwrap();
        }
        write!(out, r#"<p><b>主張</b>　{}</p><ul>"#, h(&b.claim)).unwrap();
        for step in &b.steps {
            let id = &step.id;
            let page = page_number(aux, id).map_or_else(|| "?".into(), |p| p.to_string());
            write!(
                out,
                r#"<li><a href="{file}#{id}">{id}</a>　{}　{}</li>"#,
                h(&step.title),
                pdf_link(book, aux, id, &format!("PDF p.{page}"), "pdfl")
            )
            .unwrap();
        }
        write!(out, r#"</ul><p><a class="pdfl" href="{file}">HTML を開く</a>　{}<span class="lab">　式 {} 行（うち PDF だけ {} 行、原稿に無く補った行 {} 行）</span></p></div>"#,
            pdf_link(book, aux, id, "PDF の該当ページ", "pdfl"), lines.len(),
            lines.iter().filter(|l| l.fine.is_some()).count(), lines.iter().filter(|l| l.reconstructed).count()).unwrap();
    }
    out
}

pub(super) fn render(book: &Book, aux: &Aux) -> Result<Vec<(String, String)>> {
    let (css, js) = fuzukue_reading::html::katex(None)?;
    let mut result = Vec::new();
    for block in &book.blocks {
        let html = page(
            &block_html(book, block, aux, false),
            &block.title,
            &css,
            &js,
            true,
        );
        result.push((book.block_html(&block.id), html));
    }
    result.push((
        book.stem(".html"),
        page(&full(book, aux), &book.title, &css, &js, true),
    ));
    let index = page(
        &index(book, aux),
        &format!("{}：索引", book.title),
        &css,
        &js,
        false,
    );
    result.push((book.stem("-index.html"), index));
    Ok(result)
}
