//! Offset-preserving readers for explicitly authored symbols.
use std::ops::Range;
use std::path::Path;
use std::sync::LazyLock;

use fuzukue_proto::SymbolKind::{self, *};
use regex::Regex;
use serde::Deserialize;

macro_rules! pattern {
    ($name:ident, $re:literal) => {
        static $name: LazyLock<Regex> = LazyLock::new(|| Regex::new($re).unwrap());
    };
}

pattern!(WIKI, r"\[\[([^\s\]|]+)(?:\|[^\]\r\n]*)?\]\]");
pattern!(TEX_TOKEN, r"\\[a-zA-Z@]+|\\[^\r\n]|%[^\r\n]*");
pattern!(TEX_ARG, r"^\*?\s*\{([^{}]+)\}");
pattern!(
    MACRO_ARG,
    r"^\*?\s*(?:\{\s*(\\[a-zA-Z@]+)\s*\}|(\\[a-zA-Z@]+))"
);
pattern!(MD_CODE, r"`+[^`\r\n]*`+");
pattern!(LEAN_MASK, r#"--|/-|""#);

const IDENT: &str = r"(?:[\p{Alphabetic}_][\p{Alphabetic}\p{M}\p{N}_'!?]*|«[^»\r\n]+»)";
static LEAN_ID: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(&format!(r"{IDENT}(?:\.{IDENT})*")).unwrap());
static LEAN_COMMAND: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r"(?m)^[ \t]*(?:@\[[^\]]*\]\s*)*(?:(?:private|protected|noncomputable|unsafe|partial|scoped|local)\s+)*(?:(def|theorem|lemma|structure|inductive|class|abbrev|instance|example)\b(?:[ \t]+({IDENT}(?:\.{IDENT})*))?|(namespace|section|end|open|variable|variables|parameter|parameters|import|mutual)\b)[^\r\n]*"
    )).unwrap()
});
// Bindings in declaration parameters, terms and common tactics. Within a
// command, conservatively suppress the entire span if a name is shadowed.
pattern!(
    BINDING,
    r"(?m)(?:[({⦃\[]\s*([\p{Alphabetic}_][\p{Alphabetic}\p{M}\p{N}_'!?]*(?:[ \t]+[\p{Alphabetic}_][\p{Alphabetic}\p{M}\p{N}_'!?]*)*)\s*:|\b(?:fun|∀|let|have|suffices|obtain|intro|intros|rintro|rename_i|case|next|for)\s+([^\n:;,=→]+)|=>\s*$)"
);
pattern!(BRANCH_BINDER, r"(?m)^\s*\|\s*([^|\r\n]+?)\s*=>");
pattern!(
    UNTYPED_BINDER,
    r"\(\s*([\p{Alphabetic}_][\p{Alphabetic}\p{M}\p{N}_'!?]*)\s*\)"
);

#[derive(Clone, Debug)]
pub(super) struct Mark {
    pub kind: SymbolKind,
    pub name: String,
    pub range: Range<usize>,
    pub origin: &'static str,
    pub qualified: Option<String>,
}

impl Mark {
    fn new(kind: SymbolKind, name: &str, range: Range<usize>, origin: &'static str) -> Self {
        Self {
            kind,
            name: name.to_owned(),
            range,
            origin,
            qualified: None,
        }
    }
}

#[derive(Default)]
pub(super) struct Extract {
    pub defs: Vec<Mark>,
    pub refs: Vec<Mark>,
    pub text: String,
    pub lean: Vec<LeanScope>,
    pub tex_entry: bool,
    pub includes: Vec<String>,
    pub imports: Vec<String>,
}

pub(super) fn source(path: &Path, raw: &str) -> Option<Extract> {
    match path.extension().and_then(|ext| ext.to_str()) {
        Some("md") => card(raw),
        Some("lean") => Some(lean(raw)),
        _ => Some(tex(raw)),
    }
}

fn mask_bytes(text: &mut [u8], range: Range<usize>) {
    for b in &mut text[range] {
        if !matches!(b, b'\r' | b'\n') {
            *b = b' ';
        }
    }
}

fn markdown_mask(raw: &str) -> String {
    let mut bytes = raw.as_bytes().to_vec();
    let mut fence: Option<(u8, usize)> = None;
    let mut start = 0;
    for line in raw.split_inclusive('\n') {
        let end = start + line.len();
        let trimmed = line.trim_start();
        let delimiter = trimmed
            .as_bytes()
            .first()
            .copied()
            .filter(|b| matches!(b, b'`' | b'~'));
        let run = delimiter.map(|b| (b, trimmed.bytes().take_while(|&c| c == b).count()));
        match (fence, run) {
            (None, Some((b, n))) if n >= 3 => {
                fence = Some((b, n));
                mask_bytes(&mut bytes, start..end);
            }
            (Some((b, n)), close) => {
                if close.is_some_and(|(c, m)| c == b && m >= n && trimmed[m..].trim().is_empty()) {
                    fence = None;
                }
                mask_bytes(&mut bytes, start..end);
            }
            _ => {
                for code in MD_CODE.find_iter(line) {
                    mask_bytes(&mut bytes, start + code.start()..start + code.end());
                }
            }
        }
        start = end;
    }
    String::from_utf8(bytes).unwrap()
}

#[derive(Deserialize)]
struct Front {
    id: String,
    label: Option<String>,
    #[serde(default)]
    defines: Option<Vec<String>>,
    #[serde(default)]
    lean: Vec<String>,
    #[serde(default)]
    links: Vec<fuzukue_cards::Link>,
    #[serde(default)]
    requires: Vec<String>,
    from: Option<String>,
}

fn field(front: &str, key: &str) -> Range<usize> {
    let mut start = None;
    let mut offset = 0;
    for line in front.split_inclusive('\n') {
        if !line.starts_with(char::is_whitespace) && !line.starts_with('-') && line.contains(':') {
            if let Some(start) = start {
                return start..offset;
            }
            if line
                .strip_prefix(key)
                .is_some_and(|rest| rest.starts_with(':'))
            {
                start = Some(offset + key.len() + 1);
            }
        }
        offset += line.len();
    }
    start.unwrap_or(0)..offset
}

pub(super) fn card(raw: &str) -> Option<Extract> {
    let (yaml, body) = fuzukue_cards::split_frontmatter(raw)?;
    let front: Front = serde_yaml::from_str(yaml).ok()?;
    let base = raw.find('\n')? + 1;
    let mut out = Extract::default();
    let fields = [
        ("id", Card, "card_id", vec![front.id], true),
        (
            "label",
            Label,
            "card_label",
            front.label.into_iter().collect(),
            true,
        ),
        (
            "defines",
            Term,
            "card_defines",
            front.defines.unwrap_or_default(),
            true,
        ),
        ("lean", Lean, "card_lean", front.lean, false),
        (
            "links",
            Card,
            "card_link",
            front.links.into_iter().map(|link| link.to).collect(),
            false,
        ),
        ("requires", Card, "card_requires", front.requires, false),
        (
            "from",
            Card,
            "card_from",
            front.from.into_iter().collect(),
            false,
        ),
    ];
    for (key, kind, origin, names, definition) in fields {
        if names.is_empty() {
            continue;
        }
        let field = field(yaml, key);
        let mut offset = field.start;
        for name in names.into_iter().filter(|name| !name.trim().is_empty()) {
            let range = yaml[offset..field.end].find(&name).map_or_else(
                || base + field.start..base + field.end,
                |idx| {
                    let start = offset + idx;
                    offset = start + name.len();
                    base + start..base + offset
                },
            );
            let mark = Mark::new(kind, &name, range, origin);
            if definition {
                out.defs.push(mark);
            } else {
                // A formalization both links to the Lean declaration and names
                // this card as a mathematical description of the declaration.
                if kind == Lean {
                    let mut alias = mark.clone();
                    alias.qualified = Some(name.clone());
                    alias.name = short_name(&name).to_owned();
                    out.defs.push(alias);
                }
                out.refs.push(mark);
            }
        }
    }
    let body_start = raw.len() - body.len();
    let body = markdown_mask(body);
    let text = format!("{}{}", " ".repeat(body_start), body);
    out.refs.extend(wikis(&text));
    let tex = tex_like(&text, false);
    out.defs.extend(tex.defs);
    out.refs.extend(tex.refs);
    out.includes = tex.includes;
    out.text = fuzukue_cards::tex_prose_mask(&tex.text, false);
    Some(out)
}

pub(super) fn wikis(text: &str) -> impl Iterator<Item = Mark> + '_ {
    WIKI.captures_iter(text).filter_map(|cap| {
        let name = cap.get(1)?;
        (!name.as_str().starts_with("file:"))
            .then(|| Mark::new(Card, name.as_str(), name.range(), "wikilink"))
    })
}

pub(super) fn tex(raw: &str) -> Extract {
    let mut extracted = tex_like(raw, true);
    extracted.refs.extend(wikis(&extracted.text));
    extracted
}

fn tex_like(raw: &str, comments: bool) -> Extract {
    let mut out = Extract::default();
    let mut masked = raw.as_bytes().to_vec();
    let mut consumed = 0;
    let mut preamble = true;
    let mut cursor = 0;
    while let Some(token) = TEX_TOKEN.find_at(raw, cursor) {
        cursor = token.end();
        if token.start() < consumed {
            continue;
        }
        let command = token.as_str();
        if command.starts_with('%') {
            if comments {
                mask_bytes(&mut masked, token.range());
            } else {
                cursor = token.start() + 1;
            }
            continue;
        }
        let rest = &raw[token.end()..];
        let mark = match command {
            "\\documentclass" | "\\documentstyle" => {
                out.tex_entry = true;
                continue;
            }
            "\\input" | "\\include" | "\\subfile" => {
                if let Some(arg) = TEX_ARG.captures(rest).and_then(|cap| cap.get(1)) {
                    out.includes.push(arg.as_str().trim().to_owned());
                }
                continue;
            }
            "\\label" | "\\ref" | "\\cref" | "\\Cref" | "\\eqref" | "\\autoref" | "\\nameref" | "\\pageref" | "\\vref" => {
                if let Some(arg) = TEX_ARG.captures(rest).and_then(|cap| cap.get(1)) {
                    consumed = token.end() + arg.end() + 1;
                    let mut offset = token.end() + arg.start();
                    for entry in arg.as_str().split(',') {
                        let name = entry.trim();
                        let start = offset + entry.len() - entry.trim_start().len();
                        if !name.is_empty() {
                            let definition = command == "\\label";
                            let mark = Mark::new(
                                Label,
                                name,
                                start..start + name.len(),
                                if definition { "tex_label" } else { "tex_ref" },
                            );
                            if definition {
                                out.defs.push(mark);
                            } else {
                                out.refs.push(mark);
                            }
                        }
                        offset += entry.len() + 1;
                    }
                    mask_bytes(&mut masked, token.start()..consumed);
                }
                continue;
            }
            "\\begin" => {
                if let Some(env) = TEX_ARG.captures(rest).and_then(|cap| cap.get(1)) {
                    match env.as_str() {
                        "document" => preamble = false,
                        env @ ("verbatim" | "verbatim*" | "Verbatim" | "lstlisting" | "minted"
                        | "comment") => {
                            let close = format!("\\end{{{env}}}");
                            cursor = rest
                                .find(&close)
                                .map_or(raw.len(), |offset| token.end() + offset + close.len());
                            mask_bytes(&mut masked, token.start()..cursor);
                        }
                        _ => {}
                    }
                }
                continue;
            }
            "\\verb" => {
                let content = rest.strip_prefix('*').unwrap_or(rest);
                if let Some(delimiter) = content.chars().next().filter(|ch| !ch.is_whitespace()) {
                    let after_open = &content[delimiter.len_utf8()..];
                    let size = after_open
                        .find(['\n', delimiter])
                        .unwrap_or(after_open.len());
                    cursor = raw.len() - after_open.len() + size;
                    if after_open[size..].starts_with(delimiter) {
                        cursor += delimiter.len_utf8();
                    }
                    mask_bytes(&mut masked, token.start()..cursor);
                }
                continue;
            }
            "\\newcommand" | "\\DeclareMathOperator" if preamble => {
                if let Some(arg) = MACRO_ARG
                    .captures(rest)
                    .and_then(|cap| cap.get(1).or_else(|| cap.get(2)))
                {
                    consumed = token.end() + arg.end();
                    let range = token.end() + arg.start()..consumed;
                    out.defs
                        .push(Mark::new(Macro, arg.as_str(), range, "tex_macro"));
                }
                continue;
            }
            _ if command.len() > 1 && command.as_bytes()[1].is_ascii_alphabetic() => {
                Mark::new(Macro, command, token.range(), "tex_macro_use")
            }
            _ => continue,
        };
        out.refs.push(mark);
    }
    out.text = String::from_utf8(masked).unwrap();
    out
}

pub(super) fn short_name(name: &str) -> &str {
    name.rsplit('.').next().unwrap_or(name)
}

pub(super) fn term_key(name: &str) -> String {
    name.to_ascii_lowercase()
}

pub(super) fn terms<'a>(folded: &'a str, key: &'a str) -> impl Iterator<Item = Range<usize>> + 'a {
    // ASCII terms match at word boundaries. Japanese terms are substrings.
    let ascii = key.is_ascii();
    let word = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    folded.match_indices(key).filter_map(move |(start, found)| {
        let end = start + found.len();
        let bytes = folded.as_bytes();
        (!ascii
            || (!start.checked_sub(1).is_some_and(|i| word(bytes[i]))
                && !bytes.get(end).copied().is_some_and(word)))
        .then_some(start..end)
    })
}

pub(super) struct LeanScope {
    pub range: Range<usize>,
    pub namespace: String,
    pub opens: Vec<String>,
    pub locals: Vec<String>,
    skip: Range<usize>,
}

fn lean_mask(raw: &str) -> String {
    let mut text = raw.as_bytes().to_vec();
    let mut cursor = 0;
    while let Some(m) = LEAN_MASK.find_at(raw, cursor) {
        cursor = m.end();
        match m.as_str() {
            "--" => cursor += raw[cursor..].find('\n').unwrap_or(raw.len() - cursor),
            "/-" => {
                let mut depth = 1;
                while cursor < raw.len() && depth > 0 {
                    match raw.as_bytes().get(cursor..cursor + 2) {
                        Some(b"/-") => {
                            depth += 1;
                            cursor += 2;
                        }
                        Some(b"-/") => {
                            depth -= 1;
                            cursor += 2;
                        }
                        _ => cursor += 1,
                    }
                }
            }
            _ => {
                while let Some(&byte) = raw.as_bytes().get(cursor) {
                    cursor += 1;
                    if byte == b'"' {
                        break;
                    }
                    if byte == b'\\' {
                        cursor = (cursor + 1).min(raw.len());
                    }
                }
            }
        }
        mask_bytes(&mut text, m.start()..cursor);
    }
    String::from_utf8(text).unwrap()
}

fn bindings(text: &str) -> Vec<String> {
    BINDING
        .captures_iter(text)
        .filter_map(|cap| cap.get(1).or_else(|| cap.get(2)))
        .flat_map(|names| LEAN_ID.find_iter(names.as_str()))
        .map(|name| name.as_str().to_owned())
        .collect()
}

pub(super) fn lean(raw: &str) -> Extract {
    let mut out = Extract {
        text: lean_mask(raw),
        ..Extract::default()
    };
    let mut scopes: Vec<(String, bool, usize, usize)> = Vec::new();
    let mut opens: Vec<String> = Vec::new();
    let mut variables: Vec<String> = Vec::new();
    let mut commands = LEAN_COMMAND.captures_iter(&out.text).peekable();
    let mut start = 0;
    while let Some(cap) = commands.next() {
        let command = cap.get(0).unwrap();
        let end = commands
            .peek()
            .map_or(raw.len(), |next| next.get(0).unwrap().start());
        let namespace = scopes
            .iter()
            .filter(|(_, ns, _, _)| *ns)
            .map(|(name, _, _, _)| name.as_str())
            .collect::<Vec<_>>()
            .join(".");
        if let Some(kind) = cap.get(1) {
            let name = cap
                .get(2)
                .filter(|_| kind.as_str() != "example")
                .filter(|m| !matches!(m.as_str(), "where" | "deriving"));
            let skip = command.start()..name.map_or(kind.end(), |m| m.end());
            if let Some(name) = name {
                let name_text = name.as_str();
                let full = name_text
                    .strip_prefix("_root_.")
                    .map(str::to_owned)
                    .unwrap_or_else(|| {
                        if namespace.is_empty() {
                            name_text.to_owned()
                        } else {
                            format!("{namespace}.{name_text}")
                        }
                    });
                let origin = match kind.as_str() {
                    "theorem" => "lean_theorem",
                    "lemma" => "lean_lemma",
                    "structure" => "lean_structure",
                    "inductive" => "lean_inductive",
                    "class" => "lean_class",
                    "abbrev" => "lean_abbrev",
                    "instance" => "lean_instance",
                    _ => "lean_def",
                };
                let mut mark = Mark::new(Lean, short_name(name_text), name.range(), origin);
                mark.qualified = Some(full);
                out.defs.push(mark);
            }
            let mut locals = variables.clone();
            let declaration = &out.text[skip.end..end];
            locals.extend(bindings(declaration));
            for branch in BRANCH_BINDER.captures_iter(declaration) {
                locals.extend(
                    LEAN_ID
                        .find_iter(branch.get(1).unwrap().as_str())
                        .filter(|name| !name.as_str().contains('.'))
                        .map(|name| name.as_str().to_owned()),
                );
            }
            let parameters = declaration.split([':', '=']).next().unwrap_or_default();
            locals.extend(
                UNTYPED_BINDER
                    .captures_iter(parameters)
                    .map(|cap| cap[1].to_owned()),
            );
            out.lean.push(LeanScope {
                range: command.start()..end,
                namespace,
                opens: opens.clone(),
                locals,
                skip,
            });
        } else if let Some(kind) = cap.get(3) {
            let args = out.text[kind.end()..command.end()].trim();
            let name = LEAN_ID.find(args).map(|m| m.as_str()).unwrap_or_default();
            match kind.as_str() {
                "namespace" | "section" | "mutual" => scopes.push((
                    name.to_owned(),
                    kind.as_str() == "namespace",
                    opens.len(),
                    variables.len(),
                )),
                "end" => {
                    if let Some((_, _, open_count, variable_count)) = scopes.pop() {
                        opens.truncate(open_count);
                        variables.truncate(variable_count);
                    }
                }
                "open" if !args.starts_with("scoped ") => {
                    let names = if args.contains(['(', '«'])
                        || args.contains(" hiding ")
                        || args.contains(" renaming ")
                    {
                        ""
                    } else {
                        args
                    };
                    opens.extend(LEAN_ID.find_iter(names).map(|m| m.as_str().to_owned()));
                }
                "import" => out
                    .imports
                    .extend(LEAN_ID.find_iter(args).map(|name| name.as_str().to_owned())),
                "variable" | "variables" | "parameter" | "parameters" => {
                    variables.extend(bindings(&out.text[kind.end()..end]));
                }
                _ => {}
            }
            if !matches!(kind.as_str(), "import" | "open") {
                out.lean.push(LeanScope {
                    range: command.end()..end,
                    namespace,
                    opens: opens.clone(),
                    locals: variables.clone(),
                    skip: 0..0,
                });
            }
        }
        start = end;
    }
    if start < raw.len() && out.lean.is_empty() {
        out.lean.push(LeanScope {
            range: start..raw.len(),
            namespace: String::new(),
            opens,
            locals: variables,
            skip: 0..0,
        });
    }
    out
}

pub(super) fn lean_uses<'a>(
    text: &'a str,
    scope: &'a LeanScope,
) -> impl Iterator<Item = Mark> + 'a {
    LEAN_ID
        .find_iter(&text[scope.range.clone()])
        .filter_map(move |m| {
            let name = m.as_str();
            let start = scope.range.start + m.start();
            let first = name.split('.').next().unwrap_or(name);
            (!scope.skip.contains(&start) && !scope.locals.iter().any(|local| local == first))
                .then(|| Mark::new(Lean, name, start..start + name.len(), "lean_identifier"))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn front_matter_and_wikis_preserve_utf8_locations_and_skip_code() {
        let raw = "---\r\nid: a\r\nlabel: sec:a\r\ndefines: [集中, 'a term']\r\nlean: [N.f]\r\nrequires: [b]\r\nfrom: other#old\r\nlinks: [{to: c}]\r\n---\r\n`[[bad]]` [[ns:b|B]]\n~~~\n[[bad]]\n~~~\n";
        let parsed = card(raw).unwrap();
        assert_eq!(parsed.defs.len(), 5);
        assert_eq!(parsed.refs.len(), 5);
        for mark in parsed
            .defs
            .iter()
            .chain(&parsed.refs)
            .filter(|m| m.origin != "card_lean" || m.qualified.is_none())
        {
            assert_eq!(&raw[mark.range.clone()], mark.name);
        }
    }

    #[test]
    fn tex_extracts_labels_and_macros_with_comment_and_escape_handling() {
        let raw = r"\newcommand{\foo}{\bar} \DeclareMathOperator*\Test{T}
% \label{bad}
\verb|\label{bad}| \begin{verbatim}\label{bad}\end{verbatim}
\% \label{eq:a} \Cref{eq:a, eq:b} \eqref{eq:b} \\label{bad} \foo";
        let parsed = tex(raw);
        assert_eq!(
            parsed
                .defs
                .iter()
                .map(|m| m.name.as_str())
                .collect::<Vec<_>>(),
            ["\\foo", "\\Test", "eq:a"]
        );
        assert_eq!(
            parsed
                .refs
                .iter()
                .filter(|m| m.kind == Label)
                .map(|m| &raw[m.range.clone()])
                .collect::<Vec<_>>(),
            ["eq:a", "eq:b", "eq:b"]
        );
        assert_eq!(parsed.refs.iter().filter(|m| m.name == "\\foo").count(), 1);
    }

    #[test]
    fn matches_terms_with_ascii_boundaries_and_japanese_substrings() {
        let text = term_key("Median medians MEDIAN と中央値関数");
        assert_eq!(terms(&text, "median").count(), 2);
        assert_eq!(
            terms(&text, "中央値").next().map(|range| &text[range]),
            Some("中央値")
        );
    }

    #[test]
    fn lean_namespaces_comments_and_locals() {
        let raw = r#"namespace A
/- /- def bad := 0 -/ -/
section
variable (g : Nat)
def f := 0
theorem t (f : Nat) : f = f := by
  let s := "A.f"
  let n := g
  exact Eq.refl f
end
@[simp] noncomputable abbrev u := f
instance : Inhabited Nat := ⟨0⟩
end A
def z := A.f"#;
        let parsed = lean(raw);
        assert_eq!(
            parsed
                .defs
                .iter()
                .filter_map(|m| m.qualified.as_deref())
                .collect::<Vec<_>>(),
            ["A.f", "A.t", "A.u", "z"]
        );
        let uses = parsed
            .lean
            .iter()
            .flat_map(|scope| lean_uses(&parsed.text, scope))
            .map(|m| m.name)
            .collect::<Vec<_>>();
        assert_eq!(uses.iter().filter(|name| *name == "f").count(), 1);
        assert!(!uses.iter().any(|name| name == "g" || name == "bad"));
        assert!(uses.iter().any(|name| name == "A.f"));
    }
}
