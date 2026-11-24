use crate::{Doc, Question, Site, Symbol, Use};
use regex::Regex;
use std::sync::LazyLock;

macro_rules! patterns {
    ($($name:ident = $pattern:literal;)*) => {$(
        pub static $name: LazyLock<Regex> = LazyLock::new(|| Regex::new($pattern).unwrap());
    )*};
}
patterns! {
    HEADING = r"(?m)^ {0,3}#{1,6}[ \t]+(.+)$|\\(?:sub)*section\*?\{([^{}\n]+)\}";
    MATH_HEAD = r"(?i)^\s*(?:\d+\.?\s+)?(?:\*\*)?(definition|theorem|lemma|proposition|corollary|定義|定理|補題|命題|系)(?:\s+|[:：.（(]\s*|$)(.*)$";
    MARKED_MATH = r"(?mi)^\s*(?:>\s*)?\*\*(?:definition|theorem|lemma|proposition|corollary|定義|定理|補題|命題)(?:\s+[^*\n]{1,80}|[.:：])?\*\*";
    TEX_ENV = r"\\(begin|end)\{([A-Za-z]+)\*?\}(?:[ \t]*\[([^\]\n]{1,160})\])?";
    NEW_ENV = r"\\newtheorem\*?\{([A-Za-z]+)\}(?:\[[^\]\n]+\])?\{([^{}\n]+)\}";
    MACRO_DEF = r"\\(newcommand|renewcommand|providecommand|DeclareMathOperator)\*?\s*(?:\{\s*)?\\([A-Za-z@]+)\s*\}?";
    MACRO = r"\\([A-Za-z@]+)";
    TEX_ARG = r"\\(label|[Cc]ref|(?:auto|eq|page|name|v)?ref|(?:[A-Za-z]*[Cc]ite[A-Za-z]*)|input|include)\*?\s*(?:\[[^\]\n]*\]\s*){0,2}\{([^{}\n]+)\}";
    EMPH = r"\\(?:emph|textbf|textit)\{([^{}\n]{2,100})\}|\*\*([^*\n]{2,100})\*\*";
    NAMING_EN = r#"(?i)\b(?:we call|(?:is|are) called) [^.!?\n]{0,140}?(?:\*\*([^*\n]{2,90})\*\*|\\(?:emph|textbf|textit)\{([^{}\n]{2,90})\}|“([^”\n]{2,90})”|"([^"\n]{2,90})")"#;
    PLAIN_EN = r"(?i)\b(?:we call (?:this|it|them|such (?:an? )?[^,.]{1,32})|(?:is|are) called) (?:an? |the )?([a-z][a-z0-9-]*(?: [a-z][a-z0-9-]*){0,5})[.,;:]";
    NAMING_JA = r"(?:[をは][ \t]*|[、，,][ \t]*)(?:「([^」\n]{2,65})」|『([^』\n]{2,65})』|\\(?:textbf|emph|textit)\{([^{}\n]{2,65})\}|([\p{Han}\p{Hiragana}\p{Katakana}A-Za-z0-9][\p{Han}\p{Hiragana}\p{Katakana}A-Za-z0-9・ー -]{1,38}?))[ \t]*と(?:呼ぶ|よぶ|いう|呼んで)";
    QUESTION = r"(?i)\b(?:questions?|conjectures?|open problems?|todo)\b|未解決|予想|問い|[?？]\s*$|^(?:\d+[.)]?\s*)?問題";
    TODO = r"(?mi)^[ \t]*(?:[-*][ \t]+(?:\[ \][ \t]+)?)?(?:TODO\b|未解決[：:])[^\n]*";
    WIKI = r"\[\[([^\]|\n]+)(?:\|[^\]\n]*)?\]\]";
    MD_LINK = r"\]\((?:<([^>\n]+)>|([^()\s]+))(?:[ \t]+[\x22'][^\n]*?[\x22'])?\)";
    MD_FENCE = r"(?m)^ {0,3}(`{3,}|~{3,}).*$";
    TEX_COMMENT = r"(?m)(?:^|[^\\\n])(?:\\\\)*(%[^\n]*)";
    LEAN_HEAD = r"(?m)^[ \t]*(?:@\[[^\n]*\][ \t]*)?(?:(?:private|protected|noncomputable|unsafe|partial|local)[ \t]+)*(?:((?:def|abbrev|theorem|lemma|structure|class|inductive|axiom))[ \t]+([\p{L}_«][\p{L}\p{M}\p{N}_'!?.«»₀-₉]*))|^[ \t]*(namespace|section|end)(?:[ \t]+([\w.]+))?";
    LEAN_IDENT = r"[\p{L}_][\p{L}\p{M}\p{N}_'!?]*(?:\.[\p{L}_][\p{L}\p{M}\p{N}_'!?]*)*";
    IMPORT = r"(?m)^[ \t]*(?:(?:public|private|meta) )?import[ \t]+([\w.]+)";
    LEAN_COMMENT = r#"/-|--|""#;
    BIB = r"(?mi)^\s*@(\w+)\s*[({]\s*([^\s,{}()]+)\s*,";
    TEX_HREF = r"\\href\{((?:run:|file:)[^{}\n]+)\}";
    MD_CITE = r"(?:\[|;)\s*-?@([\w:.+/-]+)";
    CODE_SPAN = r"`([^`\n]+)`";
    WORDS = r"[\p{L}\p{N}]+";
    TERM_SPLIT = r"[ _-]+";
}

pub fn normalize(s: &str) -> String {
    TERM_SPLIT
        .replace_all(&s.trim().to_lowercase(), " ")
        .into_owned()
}

fn concept(doc: &mut Doc, name: &str, offset: usize, origin: &'static str) {
    let name = name
        .trim()
        .trim_matches(['*', '"', '「', '」', '『', '』', ':', '：']);
    if (2..=90).contains(&name.chars().count())
        && WORDS.is_match(name)
        && !name.contains(['$', '\\', '`', '[', ']', '(', ')'])
    {
        doc.define("concept", name, offset, origin);
    }
}

fn math_kind(s: &str) -> Option<&'static str> {
    Some(match s.to_lowercase().as_str() {
        "definition" | "def" | "defn" | "定義" => "definition",
        "theorem" | "thm" | "定理" => "theorem",
        "lemma" | "lem" | "補題" => "lemma",
        "prop" | "proposition" | "命題" => "proposition",
        "cor" | "corollary" | "系" => "corollary",
        _ => return None,
    })
}

// Mask bytes while preserving source offsets and line numbers.
fn mask(text: &str, ranges: impl IntoIterator<Item = std::ops::Range<usize>>) -> String {
    let mut bytes = text.as_bytes().to_vec();
    for range in ranges {
        for byte in &mut bytes[range] {
            if *byte != b'\n' {
                *byte = b' ';
            }
        }
    }
    String::from_utf8(bytes).unwrap()
}

fn prose_body(text: &str, ext: &str) -> String {
    if ext == "tex" {
        return mask(
            text,
            TEX_COMMENT
                .captures_iter(text)
                .map(|m| m.get(1).unwrap().range()),
        );
    }
    if ext != "md" {
        return text.to_owned();
    }
    let mut ranges = Vec::new();
    let mut fence = None;
    for caps in MD_FENCE.captures_iter(text) {
        let line = caps.get(0).unwrap();
        let marker = &caps[1];
        if let Some((start, ch, len)) = fence {
            if marker.as_bytes()[0] == ch
                && marker.len() >= len
                && line.as_str().trim_end().ends_with(marker)
            {
                ranges.push(start..line.end());
                fence = None;
            }
        } else {
            fence = Some((line.start(), marker.as_bytes()[0], marker.len()));
        }
    }
    if let Some((start, _, _)) = fence {
        ranges.push(start..text.len());
    }
    if let Some((_, body)) = fuzukue_cards::split_frontmatter(text) {
        ranges.push(0..text.len() - body.len());
    }
    mask(text, ranges)
}

pub fn lean_code(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut ranges = Vec::new();
    let mut pos = 0;
    while let Some(m) = LEAN_COMMENT.find_at(text, pos) {
        pos = m.end();
        match m.as_str() {
            "--" => pos += text[pos..].find('\n').unwrap_or(text.len() - pos),
            "/-" => {
                let mut depth = 1;
                while pos < bytes.len() && depth > 0 {
                    match bytes.get(pos..pos + 2) {
                        Some(b"/-") => {
                            depth += 1;
                            pos += 2;
                        }
                        Some(b"-/") => {
                            depth -= 1;
                            pos += 2;
                        }
                        _ => pos += 1,
                    }
                }
            }
            _ => {
                while pos < bytes.len() {
                    let ch = bytes[pos];
                    pos += 1;
                    if ch == b'"' {
                        break;
                    }
                    if ch == b'\\' && pos < bytes.len() {
                        pos += 1;
                    }
                }
            }
        }
        ranges.push(m.start()..pos);
    }
    mask(text, ranges)
}

fn lean(doc: &mut Doc, text: &str) {
    let mut scopes = vec![];
    for caps in LEAN_HEAD.captures_iter(text) {
        if let Some(name) = caps.get(2) {
            let name = name.as_str().trim_end_matches('.');
            let namespaces: Vec<_> = scopes.iter().filter_map(|(_, ns)| *ns).collect();
            let full = if let Some(root) = name.strip_prefix("_root_.") {
                root.to_owned()
            } else {
                namespaces
                    .into_iter()
                    .chain([name])
                    .collect::<Vec<_>>()
                    .join(".")
            };
            let kind = match &caps[1] {
                "theorem" => "lean_theorem",
                "lemma" => "lean_lemma",
                "structure" => "lean_structure",
                "class" => "lean_class",
                "inductive" => "lean_inductive",
                "axiom" => "lean_axiom",
                _ => "lean_def",
            };
            doc.define(
                kind,
                &full,
                caps.get(2).unwrap().start(),
                "lean_declaration",
            );
        } else {
            let name = caps.get(4).map(|m| m.as_str());
            match &caps[3] {
                "namespace" => scopes.push((name, name)),
                "section" => scopes.push((name, None)),
                _ => {
                    if let Some(name) = name {
                        if let Some(i) = scopes.iter().rposition(|(scope, _)| *scope == Some(name))
                        {
                            scopes.truncate(i);
                        }
                    } else {
                        scopes.pop();
                    }
                }
            }
        }
    }
    for caps in IMPORT.captures_iter(text) {
        let m = caps.get(1).unwrap();
        doc.refer("lean_import", m.as_str(), m.start());
    }
}

fn cards(doc: &mut Doc, raw: &str) {
    if fuzukue_cards::split_frontmatter(raw).is_none() {
        return;
    }
    let path = std::path::Path::new(&doc.path);
    let Ok(card) = fuzukue_cards::parse_card(std::path::Path::new(""), path, raw) else {
        return;
    };
    let front = fuzukue_cards::split_frontmatter(raw).unwrap().0;
    let offset = |s: &str| front.find(s).map_or(0, |i| i + 4);
    doc.define("card", &card.id, offset(&card.id), "card_id");
    if let Some(kind) = math_kind(&card.kind) {
        let name = if card.title.is_empty() {
            &card.id
        } else {
            &card.title
        };
        doc.define(kind, name, offset(name), "card_kind");
        if kind == "definition" {
            concept(doc, name, offset(name), "card_title");
        }
    }
    if let Some(label) = card.label {
        doc.define("label", &label, offset(&label), "card_label");
    }
    for term in card.defines {
        concept(doc, &term, offset(&term), "card_defines");
    }
    for link in card.links {
        // The body scanner owns wikilinks, including their line numbers.
        if link.rel != "mentions" || front.contains(&link.to) {
            doc.refer("card_link", &link.to, offset(&link.to));
        }
    }
    for name in card.lean {
        doc.refer("card_lean", &name, offset(&name));
    }
}

pub fn extract(doc: &mut Doc, raw: &str) -> String {
    if doc.ext == "lean" {
        let code = lean_code(raw);
        lean(doc, &code);
        return code;
    }
    if doc.ext == "bib" {
        for m in BIB.captures_iter(raw) {
            if !matches!(
                m[1].to_lowercase().as_str(),
                "comment" | "string" | "preamble"
            ) {
                doc.define("bib_entry", &m[2], m.get(2).unwrap().start(), "bib_key");
            }
        }
        return String::new();
    }
    if doc.ext == "md" {
        cards(doc, raw);
    }
    let body = prose_body(raw, &doc.ext);
    let mut envs = std::collections::HashMap::new();
    for m in NEW_ENV.captures_iter(&body) {
        if let Some(kind) = math_kind(&m[2]) {
            envs.insert(m.get(1).unwrap().as_str(), kind);
        }
    }
    let events: Vec<_> = TEX_ENV.captures_iter(&body).collect();
    for (i, m) in events.iter().enumerate() {
        if &m[1] != "begin" {
            continue;
        }
        let Some(kind) = envs.get(&m[2]).copied().or_else(|| math_kind(&m[2])) else {
            continue;
        };
        let start = m.get(0).unwrap();
        let end = events[i + 1..]
            .iter()
            .find(|n| &n[1] == "end" && n[2] == m[2])
            .map_or(body.len(), |n| n.get(0).unwrap().start());
        let content = &body[start.end()..end];
        let label = TEX_ARG.captures_iter(content).find(|n| &n[1] == "label");
        let title = m
            .get(3)
            .map(|n| n.as_str())
            .or_else(|| label.as_ref().map(|n| n.get(2).unwrap().as_str()));
        doc.define(
            kind,
            title.unwrap_or(&m[2]),
            start.start(),
            "tex_environment",
        );
        if kind == "definition" {
            if let Some(title) = m.get(3) {
                concept(doc, title.as_str(), title.start(), "tex_title");
            }
            for n in EMPH.captures_iter(content) {
                let term = n.get(1).or_else(|| n.get(2)).unwrap();
                concept(
                    doc,
                    term.as_str(),
                    start.end() + term.start(),
                    "definition_emphasis",
                );
            }
        }
    }
    for m in MACRO_DEF.captures_iter(&body) {
        doc.define("macro", &m[2], m.get(2).unwrap().start(), "tex_macro");
    }
    for m in TEX_ARG.captures_iter(&body) {
        let arg = m.get(2).unwrap();
        match &m[1] {
            "label" => doc.define("label", arg.as_str(), arg.start(), "tex_label"),
            "include" | "input" => doc.refer("tex_include", arg.as_str(), arg.start()),
            command => {
                let kind = if command.to_ascii_lowercase().contains("cite") {
                    "citation"
                } else {
                    "tex_ref"
                };
                let mut offset = arg.start();
                for item in arg.as_str().split(',') {
                    doc.refer(
                        kind,
                        item.trim(),
                        offset + item.len() - item.trim_start().len(),
                    );
                    offset += item.len() + 1;
                }
            }
        }
    }
    for m in NAMING_EN
        .captures_iter(&body)
        .chain(NAMING_JA.captures_iter(&body))
        .chain(PLAIN_EN.captures_iter(&body))
    {
        let name = m.iter().skip(1).flatten().next().unwrap();
        concept(doc, name.as_str(), name.start(), "naming_phrase");
    }
    let headings: Vec<_> = HEADING.captures_iter(&body).collect();
    for (i, m) in headings.iter().enumerate() {
        let heading = m.get(1).or_else(|| m.get(2)).unwrap();
        let title = heading.as_str().trim().trim_end_matches('#').trim_end();
        let end = headings
            .get(i + 1)
            .map_or(body.len(), |next| next.get(0).unwrap().start());
        if QUESTION.is_match(title) {
            doc.question(title, heading.start(), end);
        }
        if let Some(parts) = MATH_HEAD.captures(title) {
            let kind = math_kind(&parts[1]).unwrap();
            doc.define(kind, title, heading.start(), "heading");
            if kind == "definition" {
                let rest = parts.get(2).unwrap();
                let term = rest
                    .as_str()
                    .trim_start_matches(|c: char| c.is_ascii_digit() || ".:： ".contains(c));
                let term = term.trim_matches(['(', ')', '（', '）']);
                let offset = heading.start() + heading.as_str().find(term).unwrap_or(0);
                concept(doc, term, offset, "definition_heading");
                let content_start = m.get(0).unwrap().end();
                for e in EMPH.captures_iter(&body[content_start..end]).take(5) {
                    let term = e.get(1).or_else(|| e.get(2)).unwrap();
                    concept(
                        doc,
                        term.as_str(),
                        content_start + term.start(),
                        "definition_emphasis",
                    );
                }
            }
        }
    }
    for m in MARKED_MATH.find_iter(&body) {
        let title = m
            .as_str()
            .trim_start_matches([' ', '>', '*', '\n', '\t'])
            .trim_end_matches('*');
        if let Some(parts) = MATH_HEAD.captures(title) {
            doc.define(
                math_kind(&parts[1]).unwrap(),
                title,
                m.start() + m.as_str().find(title).unwrap(),
                "bold_statement",
            );
        }
    }
    for m in TODO.find_iter(&body) {
        doc.question(m.as_str().trim(), m.start(), m.end());
    }
    for m in WIKI.captures_iter(&body) {
        let target = m.get(1).unwrap();
        doc.refer("wikilink", target.as_str().trim(), target.start());
    }
    for m in MD_LINK.captures_iter(&body) {
        let target = m.get(1).or_else(|| m.get(2)).unwrap();
        doc.refer("file_link", target.as_str(), target.start());
    }
    for m in TEX_HREF.captures_iter(&body) {
        let target = m.get(1).unwrap();
        doc.refer("file_link", target.as_str(), target.start());
    }
    for m in MD_CITE.captures_iter(&body) {
        let target = m.get(1).unwrap();
        doc.refer("citation", target.as_str(), target.start());
    }
    body
}

impl Doc {
    fn site(&self, offset: usize) -> Site {
        let i = self.lines.partition_point(|&line| line <= offset) - 1;
        Site {
            line: i + 1,
            column: offset - self.lines[i] + 1,
        }
    }

    fn define(&mut self, kind: &'static str, name: &str, offset: usize, origin: &'static str) {
        self.symbols.push(Symbol {
            kind,
            name: name.to_owned(),
            site: self.site(offset),
            origin,
        });
    }

    pub fn refer(&mut self, kind: &'static str, name: &str, offset: usize) {
        if !name.is_empty() {
            self.uses.push(Use {
                kind,
                name: name.to_owned(),
                site: self.site(offset),
                target: None,
            });
        }
    }

    fn question(&mut self, title: &str, start: usize, end: usize) {
        let line = self.site(start).line;
        self.questions.push(Question {
            line,
            end: self.site(end.saturating_sub(1)).line.min(line + 17),
            title: title.chars().take(160).collect(),
        });
    }
}
