use super::*;

const PREAMBLE: &str = include_str!("../assets/proof-preamble.tex");

fn math(value: &str) -> String {
    static DIFF: LazyLock<FancyRegex> =
        LazyLock::new(|| FancyRegex::new(r"\\,d([st])(?![a-zA-Z])").unwrap());
    DIFF.replace_all(value, |m: &fancy_regex::Captures<'_, str>| {
        format!(r"\,\textup{{d}}{}", &m[1])
    })
    .into_owned()
}

pub(super) fn text(value: &str) -> String {
    let mut out = String::new();
    for (i, segment) in value.split('$').enumerate() {
        if i % 2 == 1 {
            write!(out, "${}$", math(segment)).unwrap();
            continue;
        }
        let mut escaped = String::new();
        for char in segment.chars() {
            if ['%', '&', '#', '_'].contains(&char) {
                escaped.push('\\');
            }
            escaped.push(char);
        }
        let lines = LINE_REF.replace_all(escaped.as_str(), |m: &fancy_regex::Captures<'_, str>| {
            let id = m.get(1).or_else(|| m.get(2)).unwrap().as_str();
            format!(r"\eqref{{{id}}}")
        });
        let steps = STEP_REF.replace_all(lines.as_ref(), |m: &fancy_regex::Captures<'_, str>| {
            format!(r"\ref{{{}}}", &m[1])
        });
        let blocks = BLOCK_REF.replace_all(steps.as_ref(), |m: &fancy_regex::Captures<'_, str>| {
            let b = &m[1];
            match m.get(2) {
                Some(n) => format!(
                    r"\hyperref[{b}.{}]{{ブロック~{b}・段~{}}}",
                    n.as_str(),
                    n.as_str()
                ),
                None => format!(r"\hyperref[blk-{b}]{{ブロック~{b}}}"),
            }
        });
        out.push_str(&blocks);
    }
    out
}

fn plain(title: &str) -> String {
    static MATH: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\$[^$]*\$").unwrap());
    MATH.replace_all(title, "")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace("（）", "")
        .replace('[', "(")
        .replace(']', ")")
}

fn chips(ids: &Value) -> String {
    list(ids)
        .iter()
        .filter_map(Value::as_u64)
        .map(|n| format!(r"\hyperref[diff-{n}]{{\textcolor{{orange!70!black}}{{[難所~{n}]}}}}~"))
        .collect()
}

fn step_context(book: &Book, step: &Step) -> String {
    let d = book.step_context(&step.id);
    let mut rows = String::new();
    if !blank(&d["why"]) {
        writeln!(
            rows,
            r"\textbf{{\textcolor{{blue}}{{なぜここで要るか}}}}\quad {} {}\par",
            text(string(d, "why")),
            chips(&d["diffs"])
        )
        .unwrap();
    }
    for (caption, links) in [("使われる先", &book.used_by), ("前提にする段", &book.uses)]
    {
        if let Some(ids) = links.get(&step.id) {
            let links = ids
                .iter()
                .map(|id| format!(r"\hyperref[{id}]{{{id}}}"))
                .collect::<Vec<_>>()
                .join(r"\quad ");
            writeln!(
                rows,
                r"\textbf{{\textcolor{{blue}}{{{caption}}}}}\quad {links}\par"
            )
            .unwrap();
        }
    }
    if let Some(class) = book.classification(step) {
        writeln!(
            rows,
            r"\textbf{{\textcolor{{blue}}{{分類}}}}\quad 台帳 {}：{class}\par",
            text(&step.ledger)
        )
        .unwrap();
    }
    rows.push_str(&review::tex_step(book, &step.id));
    if rows.is_empty() {
        return rows;
    }
    format!(
        r"\begin{{tcolorbox}}[colback=blue!4,colframe=blue!25,boxrule=.4pt,left=5pt,right=5pt,top=2pt,bottom=2pt,breakable,fontupper=\small]{rows}\end{{tcolorbox}}"
    )
}

fn formula_anchors(block: &Block) -> BTreeMap<&str, &str> {
    let mut result = BTreeMap::new();
    for step in &block.steps {
        let mut first = None;
        for item in &step.items {
            let Some(line) = item.formula() else {
                first = None;
                continue;
            };
            if line.fine.is_some() {
                let shared = *first.get_or_insert(line.id.as_str());
                result.insert(line.id.as_str(), shared);
            } else {
                first = None;
                result.insert(line.id.as_str(), line.id.as_str());
            }
        }
    }
    result
}

fn chain(
    rows: &[&Formula],
    book: &Book,
    url: &str,
    anchors: &BTreeMap<&str, &str>,
    split: &BTreeSet<String>,
    out: &mut String,
) {
    if !rows[0].lead.is_empty() {
        writeln!(out, "{}", text(&rows[0].lead)).unwrap();
    }
    writeln!(out, r"\begin{{align}}").unwrap();
    for (i, line) in rows.iter().enumerate() {
        if i > 0 {
            writeln!(out, r"\\").unwrap();
            if !line.lead.is_empty() {
                writeln!(out, r"\intertext{{{}}}", text(&line.lead)).unwrap();
            }
        }
        let id = &line.id;
        let target = anchors[id.as_str()];
        let mut mark = String::new();
        if line.reconstructed {
            mark.push_str(r"$^{\dagger}$");
        }
        if line.fine.is_some() {
            mark.push_str("細部");
        }
        let note = format!(
            r"&&\text{{\scriptsize {mark}\ \urilink{{{url}}}{{{target}}}{{↗}}\proofmark{{{id}}}}}"
        );
        let tag = if book.numbers.contains_key(id) {
            format!(r"\label{{{id}}}")
        } else {
            r"\notag".into()
        };
        let formula = math(&line.tex).replace(r"\newrow", "\\notag\\\\\n&\\quad ");
        let cuts = relations(&formula);
        if split.contains(id) && !formula.contains(r"\begin") && cuts.len() > 1 {
            let head = &formula[..cuts[0]];
            if i == 0 && !head.trim().is_empty() {
                out.push_str(head);
            } else if !head.trim().is_empty() {
                write!(out, "{{}}&{formula}{note}{tag}").unwrap();
                continue;
            }
            for (j, &start) in cuts.iter().enumerate() {
                let end = cuts.get(j + 1).copied().unwrap_or(formula.len());
                write!(out, "&{}", &formula[start..end]).unwrap();
                if j + 1 < cuts.len() {
                    writeln!(out, r"\notag\\").unwrap();
                }
            }
        } else if i > 0 {
            write!(out, "&{formula}").unwrap();
        } else if let Some(cut) = cuts.first().copied() {
            write!(out, "{}&{}", &formula[..cut], &formula[cut..]).unwrap();
        } else {
            write!(out, "{{}}&{formula}").unwrap();
        }
        write!(out, "{note}{tag}").unwrap();
    }
    writeln!(out, "\n\\end{{align}}").unwrap();
}

fn lines(
    step: &Step,
    book: &Book,
    url: &str,
    anchors: &BTreeMap<&str, &str>,
    split: &BTreeSet<String>,
    out: &mut String,
) {
    let mut run = Vec::new();
    for item in &step.items {
        let formula = item.formula();
        if !run.is_empty() && !formula.is_some_and(|line| continuation(&line.tex)) {
            chain(&run, book, url, anchors, split, out);
            run.clear();
        }
        if let Some(line) = formula {
            run.push(line);
        } else {
            writeln!(out, r"{}\par", text(item.text())).unwrap();
        }
    }
    if !run.is_empty() {
        chain(&run, book, url, anchors, split, out);
    }
}

fn context(book: &Book, out: &mut String) {
    writeln!(out, r"\phantomsection\hypertarget{{problem}}{{}}\section*{{この問題と、どこが難しいか}}\addcontentsline{{toc}}{{section}}{{この問題と、どこが難しいか}}").unwrap();
    for (caption, key) in [
        ("問題設定", "setting"),
        ("何を示すか", "goal"),
        ("難所（素直にやると何が起きるか）", "diffs"),
        ("戦略（原稿が主張する工夫）", "strategy"),
    ] {
        writeln!(out, r"\paragraph{{\textcolor{{blue}}{{{caption}}}}}").unwrap();
        if key != "diffs" {
            for line in list(&book.context["page"][key]) {
                writeln!(out, r"{}\par", text(strval(line))).unwrap();
            }
            continue;
        }
        for n in book.difficulty_ids() {
            let d = book.diff(&json!(n));
            let blocks = book
                .blocks
                .iter()
                .filter(|b| list(&book.block_context(&b.id)["diffs"]).contains(&json!(n)))
                .map(|b| format!(r"\hyperref[blk-{0}]{{{0}}}", b.id))
                .collect::<Vec<_>>()
                .join("、");
            writeln!(
                out,
                r"\phantomsection\hypertarget{{diff-{n}}}{{}}\label{{diff-{n}}}"
            )
            .unwrap();
            writeln!(out, r"\begin{{tcolorbox}}[colback=orange!5,colframe=orange!35,boxrule=.4pt,left=5pt,right=5pt,top=2pt,bottom=2pt,breakable]\textbf{{難所~{n}：{}}}\par {}\par{{\small 受け持つブロック：{blocks}}}\end{{tcolorbox}}",
                text(string(d, "title")), text(string(d, "text"))).unwrap();
        }
    }
}

pub(super) fn render(book: &Book, url: &str, split: &BTreeSet<String>) -> String {
    let mut out = PREAMBLE.to_owned();
    writeln!(out, r"{{\LARGE\bfseries {}}}\par", text(&book.title)).unwrap();
    writeln!(out, r"{{\small 読むための PDF（論文の形ではない）。対応する HTML：\urilink{{{url}}}{{top}}{{HTML を開く}}。式の右の「↗」は HTML の同じ式へ、HTML の「PDF ↗」はこの PDF の同じ式へ移る（同じフォルダに置いて使う）。リンクが開かないときは、HTML と PDF を同じフォルダに置いて開き直す。式の右の † は、原稿に無く、読解のために補った行。}}\par").unwrap();
    writeln!(out, r"\tableofcontents").unwrap();
    writeln!(out, r"\clearpage").unwrap();
    context(book, &mut out);
    out.push_str(&review::tex_front(book));
    for block in &book.blocks {
        let anchors = formula_anchors(block);
        let id = &block.id;
        let context = book.block_context(id);
        writeln!(out, r"\clearpage").unwrap();
        writeln!(out, r"\renewcommand{{\thesection}}{{{id}}}").unwrap();
        writeln!(
            out,
            r"\section[{}]{{{}\hfill{{\normalfont\small\urilink{{{url}}}{{blk-{id}}}{{↗ HTML}}}}}}",
            text(&plain(&block.title)),
            text(&block.title)
        )
        .unwrap();
        writeln!(
            out,
            r"\phantomsection\hypertarget{{blk-{id}}}{{}}\label{{blk-{id}}}\label{{{id}}}"
        )
        .unwrap();
        writeln!(out, r"{{\small 原稿：{}}}\par", text(&block.loc)).unwrap();
        if !blank(&context["role"]) {
            writeln!(out, r"\begin{{tcolorbox}}[colback=blue!4,colframe=blue!25,boxrule=.4pt,left=5pt,right=5pt,top=2pt,bottom=2pt,fontupper=\small]\textbf{{\textcolor{{blue}}{{このブロックの役割}}}}\quad {} {}\end{{tcolorbox}}",
                text(string(context, "role")), chips(&context["diffs"])).unwrap();
        }
        writeln!(out, "{}", review::tex_block(book, id)).unwrap();
        writeln!(out, r"\textbf{{主張}}\quad {}\par", text(&block.claim)).unwrap();
        for (prose, _) in &block.intro {
            writeln!(out, r"{}\par", text(prose)).unwrap();
        }
        for step in &block.steps {
            let id = &step.id;
            writeln!(out, r"\bigskip").unwrap();
            writeln!(out, r"\subsection[{}]{{{}\hfill{{\normalfont\small\urilink{{{url}}}{{{id}}}{{↗ HTML}}}}}}", text(&plain(&step.title)), text(&step.title)).unwrap();
            writeln!(
                out,
                r"\phantomsection\hypertarget{{{id}}}{{}}\label{{{id}}}"
            )
            .unwrap();
            writeln!(out, "{}", step_context(book, step)).unwrap();
            writeln!(out, r"{{\small 原稿：{}}}\par", text(&step.loc)).unwrap();
            writeln!(out, r"\begin{{lemma}}").unwrap();
            writeln!(out, "{}", text(&step.claim)).unwrap();
            writeln!(out, r"\end{{lemma}}").unwrap();
            writeln!(out, r"\begin{{proof}}").unwrap();
            lines(step, book, url, &anchors, split, &mut out);
            writeln!(out, r"\end{{proof}}").unwrap();
        }
    }
    writeln!(out, r"\end{{document}}").unwrap();
    out
}

pub(super) fn compile(dir: &Path, builder: &str) -> Result<()> {
    let cache = dir.join(".texcache");
    fs::create_dir_all(&cache)?;
    let mut argv = if builder == "latexmk" {
        [
            "latexmk",
            "-norc",
            "-g",
            "-lualatex",
            "-no-shell-escape",
            "-interaction=nonstopmode",
            "-halt-on-error",
            "-outdir=.temp",
            "main.tex",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect()
    } else {
        shlex::split(builder).context("invalid LaTeX builder command")?
    };
    for argument in &mut argv {
        if argument.contains("{tex}") {
            *argument = argument.replace("{tex}", &dir.join("main.tex").to_string_lossy());
        }
    }
    let (program, arguments) = argv.split_first().context("empty LaTeX builder command")?;
    let times = if builder == "latexmk" { 1 } else { 2 };
    for _ in 0..times {
        command(
            Command::new(program)
                .args(arguments)
                .current_dir(dir)
                .env("TEXMFCACHE", &cache),
        )
        .context("LaTeX build failed")?;
    }
    Ok(())
}

pub(super) fn overfull(source: &str, log: &str) -> Result<BTreeSet<String>> {
    let pattern = Regex::new(r"Overfull \\hbox \(([\d.]+)pt too wide\) detected at line (\d+)")?;
    let mark = Regex::new(r"\\proofmark\{([^}]*)\}")?;
    let lines: Vec<_> = source.lines().collect();
    let mut split = BTreeSet::new();
    for m in pattern.captures_iter(log) {
        if m[1].parse::<f64>()? < 3.0 {
            continue;
        }
        let index = m[2].parse::<usize>()?.saturating_sub(1);
        if index >= lines.len() {
            continue;
        }
        let Some(first) = (0..=index)
            .rev()
            .find(|&i| lines[i].contains(r"\begin{align}"))
        else {
            continue;
        };
        let Some(last) = (index..lines.len()).find(|&i| lines[i].contains(r"\end{align}")) else {
            continue;
        };
        for line in &lines[first..=last] {
            split.extend(mark.captures_iter(line).map(|m| m[1].to_owned()));
        }
    }
    Ok(split)
}
