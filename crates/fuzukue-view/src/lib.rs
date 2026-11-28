//! fuzukue の純粋な表示ロジック。GPUI に依存しないので Linux でテストできる。

pub mod appearance;
pub mod chat;
pub mod comment_anchor;
pub mod component_draft;
pub mod lean;
pub mod markdown_code;
mod paper;
pub mod server;
pub mod source_file;
pub mod style_notes;
mod tex;
pub mod ui_kit;
pub mod usage;
pub mod xml_components;

pub use paper::{paper_markdown, theorem_heading, theorem_heading_with_label};
pub use tex::{math_markdown, read_card_tex, TexMarkdown, ITEM_LABEL_PREFIX};

use std::{
    borrow::Cow,
    ops::{Range, RangeInclusive},
    path::{Path, PathBuf},
};

use percent_encoding::{percent_decode_str, utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};

const FILE_PATH_ENCODE_SET: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'/')
    .remove(b'.')
    .remove(b'-')
    .remove(b'_')
    .remove(b'~');

/// Encode a project-relative file path for a fuzukue-file link or an HTTP request.
pub fn encode_file_path(path: &str) -> String {
    utf8_percent_encode(path, FILE_PATH_ENCODE_SET).to_string()
}

/// Return the editor language token for a project-relative file path.
pub fn source_language(path: &str) -> &'static str {
    source_language_with_text(path, "")
}

/// Prefer a recognised filename, then examine the first-line interpreter.
pub fn source_language_with_text(path: &str, text: &str) -> &'static str {
    let path = Path::new(path);
    match path.file_name().and_then(|name| name.to_str()) {
        Some("Makefile" | "makefile" | "GNUmakefile") => return "make",
        Some("CMakeLists.txt") => return "cmake",
        Some(".bashrc" | ".bash_profile" | ".profile") => return "bash",
        _ => {}
    }
    if path.extension().is_some_and(|ext| ext == "C") {
        return "cpp";
    }
    let extension = path.extension().and_then(|ext| ext.to_str()).map(str::to_ascii_lowercase);
    match extension.as_deref() {
        Some("rs") => "rust",
        Some("py" | "pyw" | "pyi") => "python",
        Some("c" | "h") => "c",
        Some("cc" | "cpp" | "cxx" | "hh" | "hpp" | "hxx" | "ipp") => "cpp",
        Some("go") => "go",
        Some("java") => "java",
        Some("js" | "mjs" | "cjs" | "jsx") => "javascript",
        Some("ts" | "mts" | "cts") => "typescript",
        Some("tsx") => "tsx",
        Some("json" | "jsonc") => "json",
        Some("toml") => "toml",
        Some("yaml" | "yml") => "yaml",
        Some("md" | "markdown" | "mdown") => "markdown",
        Some("sh" | "bash") => "bash",
        Some("mk") => "make",
        Some("cmake") => "cmake",
        Some("sql") => "sql",
        Some("htm" | "html" | "xhtml") => "html",
        Some("css") => "css",
        Some("lua") => "lua",
        Some("zig" | "zon") => "zig",
        Some("kt" | "kts") => "kotlin",
        Some("swift") => "swift",
        Some("scala" | "sc") => "scala",
        Some("rb" | "rake" | "gemspec") => "ruby",
        Some("r") => "r",
        Some("jl") => "julia",
        Some("lean") => "lean",
        Some("yu") => "yulang",
        Some("tex" | "ltx" | "sty" | "cls") => "tex",
        _ => shebang_language(text).unwrap_or("text"),
    }
}

fn shebang_language(text: &str) -> Option<&'static str> {
    let mut words = text.lines().next()?.strip_prefix("#!")?.split_whitespace();
    let command = words.next()?.rsplit('/').next()?;
    let command = if command == "env" {
        words.find(|word| !word.starts_with('-') && !word.contains('='))?
    } else {
        command
    };
    match command.trim_end_matches(|ch: char| ch.is_ascii_digit() || ch == '.') {
        "bash" | "sh" | "dash" | "ksh" | "zsh" => Some("bash"),
        "python" | "pypy" => Some("python"),
        "ruby" => Some("ruby"),
        "node" | "nodejs" | "deno" | "bun" => Some("javascript"),
        "lua" | "luajit" => Some("lua"),
        "Rscript" => Some("r"),
        "julia" => Some("julia"),
        "lean" => Some("lean"),
        "swift" => Some("swift"),
        "scala" => Some("scala"),
        _ => None,
    }
}

/// Split a file reference into its path and an optional inclusive, one-based line range.
/// A suffix that is not a valid line anchor remains part of the path.
pub fn parse_file_ref(target: &str) -> (&str, Option<RangeInclusive<usize>>) {
    fn line_number(s: &str) -> Option<usize> {
        if !s.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        s.parse().ok().filter(|&line| line > 0)
    }

    let Some((path, anchor)) = target.rsplit_once('#') else {
        return (target, None);
    };
    let range = anchor.strip_prefix('L').and_then(|lines| {
        let (first, last) = lines.split_once('-').unwrap_or((lines, lines));
        let start = line_number(first)?;
        let end = line_number(last.strip_prefix('L').unwrap_or(last))?;
        (start <= end && !path.is_empty()).then_some(start..=end)
    });
    match range {
        Some(range) => (path, Some(range)),
        None => (target, None),
    }
}

/// Decode the destination of a fuzukue-file link. Split off the anchor before
/// decoding so an encoded '#' in a filename stays part of the path.
pub fn decode_file_link(target: &str) -> Option<(Cow<'_, str>, Option<RangeInclusive<usize>>)> {
    let (path, lines) = parse_file_ref(target);
    if path.is_empty() {
        return None;
    }
    Some((percent_decode_str(path).decode_utf8().ok()?, lines))
}

/// Remaining whole minutes, rounded up. Both timestamps are Unix seconds.
pub fn minutes_until(at: i64, now: i64) -> i64 {
    let seconds = at.saturating_sub(now).max(0);
    seconds / 60 + i64::from(seconds % 60 != 0)
}

/// カード本文の `[[id]]` / `[[id|表示]]` / `[[file:path#L10-20]]` を Markdown のリンクにする。
/// コードブロックとインラインコードの中は触らない。
pub fn link_card_refs(body: &str) -> String {
    link_card_refs_with(body, |_, _| None)
}

fn link_card_refs_with(body: &str, card_label: impl Fn(&str, Option<&str>) -> Option<String>) -> String {
    if !body.contains("[[") {
        return body.to_owned();
    }
    link_card_refs_mapped(body, card_label).markdown
}

/// Prepare a Markdown card for the reader while preserving its source offsets.
pub fn read_card_markdown(body: &str, refs: Option<&fuzukue_proto::DocReferences>) -> TexMarkdown {
    let cards = refs
        .filter(|_| body.contains("[["))
        .map(paper::card_references)
        .unwrap_or_default();
    link_card_refs_mapped(body, |id, label| match label {
        Some(_) => None,
        None => cards.get(id).map(|reference| paper::numbered(reference, false)),
    })
}

fn link_card_refs_mapped(
    body: &str,
    card_label: impl Fn(&str, Option<&str>) -> Option<String>,
) -> TexMarkdown {
    let mut out = TexMarkdown::default();
    let mut copied = 0;
    if body.contains("[[") {
        for_card_ref(body, |range, inner| {
            out.push_mapped(&body[copied..range.start], copied..range.start, true);
            let link = card_ref_link(inner, &card_label);
            copied = range.end;
            out.push_mapped(&link, range, false);
        });
    }
    out.push_mapped(&body[copied..], copied..body.len(), true);
    out
}

fn for_card_ref(body: &str, mut visit: impl FnMut(Range<usize>, &str)) {
    let mut start = 0;
    for code in markdown_code::link_ranges(body) {
        card_ref_ranges(&body[start..code.range.start], start, &mut visit);
        start = code.range.end;
    }
    card_ref_ranges(&body[start..], start, &mut visit);
}

fn card_ref_ranges(
    body: &str,
    mut offset: usize,
    visit: &mut impl FnMut(Range<usize>, &str),
) {
    for line in body.split_inclusive('\n') {
        let mut rest = line;
        while let Some(start) = rest.find("[[") {
            let tail = &rest[start + 2..];
            let Some((inner, after)) = tail.split_once("]]") else { break };
            let first = offset + line.len() - rest.len() + start;
            let end = offset + line.len() - after.len();
            visit(first..end, inner);
            rest = after;
        }
        offset += line.len();
    }
}

fn card_ref_link(inner: &str, card_label: &impl Fn(&str, Option<&str>) -> Option<String>) -> String {
    let (target, label) = inner
        .split_once('|')
        .map_or((inner, None), |(target, label)| (target, Some(label)));
    if let Some(path) = target.strip_prefix("file:") {
        let (file, lines) = parse_file_ref(path);
        let mut url = encode_file_path(file);
        if lines.is_some() {
            url.push_str(&path[file.len()..]);
        }
        let mut escaped_label = String::new();
        for ch in label.unwrap_or(file).chars() {
            if matches!(ch, '\\' | '[' | ']' | '*' | '_' | '<' | '>' | '&' | '$' | '~') {
                escaped_label.push('\\');
            }
            escaped_label.push(ch);
        }
        format!("[{escaped_label}](fuzukue-file:{url})")
    } else {
        let numbered = card_label(target, label);
        let label = numbered.as_deref().or(label).unwrap_or(target);
        format!("[{label}](fuzukue-card:{target})")
    }
}

/// PdfView はローカルのパスから開くので、Windows 側の一時フォルダに置く。
pub fn cache_path(rel: &str) -> PathBuf {
    let name: String = rel.chars().map(|c| if c == '/' || c == '\\' || c == ':' { '_' } else { c }).collect();
    std::env::temp_dir().join("fuzukue").join(name)
}

/// 左ペインでの種類の並び順。ここに無い種類は後ろに名前順。
pub const KIND_ORDER: &[&str] = &["plan", "goal", "decision", "question", "idea", "claim", "spec", "design", "task", "expl", "definition", "lemma", "proposition", "theorem", "proof", "example", "note"];

pub fn kind_rank(kind: &str) -> (usize, String) {
    (KIND_ORDER.iter().position(|k| *k == kind).unwrap_or(KIND_ORDER.len()), kind.to_string())
}

pub fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n).collect::<String>() + "…"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_and_labels() {
        assert_eq!(link_card_refs("see [[a]] and [[b|表示]]"), "see [a](fuzukue-card:a) and [表示](fuzukue-card:b)");
    }

    #[test]
    fn markdown_card_ranges_survive_wiki_links_and_code() {
        let body = "## 見出し\n\n[[first|前]]、`[[literal]]`、[[file:docs/a.md#L2]]の後\n\n    [[indented]]";
        let display = read_card_markdown(body, None);
        assert_eq!(display.markdown, link_card_refs(body));
        assert!(display.markdown.contains("`[[literal]]`"));
        assert!(display.markdown.ends_with("    [[indented]]"));
        let range = |text: &str, fragment: &str| {
            let start = text.find(fragment).unwrap();
            start..start + fragment.len()
        };
        for (original, rendered) in [
            ("見出し", "見出し"),
            ("[[first|前]]", "[前](fuzukue-card:first)"),
            ("[[file:docs/a.md#L2]]", "[docs/a.md](fuzukue-file:docs/a.md#L2)"),
            ("の後", "の後"),
        ] {
            let original = range(body, original);
            let rendered = range(&display.markdown, rendered);
            assert_eq!(display.tex_range(rendered.clone()), Some(original.clone()));
            assert_eq!(display.markdown_range(original), Some(rendered));
        }
    }

    #[test]
    fn file_refs_become_links() {
        assert_eq!(link_card_refs("[[file:src/x.rs]]"), "[src/x.rs](fuzukue-file:src/x.rs)");
        assert_eq!(link_card_refs("[[file:src/x.rs#L10-20]]"), "[src/x.rs](fuzukue-file:src/x.rs#L10-20)");
        assert_eq!(
            link_card_refs("[[file:docs/数式 (1)#draft.tex#L2|原稿]]"),
            "[原稿](fuzukue-file:docs/%E6%95%B0%E5%BC%8F%20%281%29%23draft.tex#L2)"
        );
        assert_eq!(
            link_card_refs("[[file:docs/a_[1].md]]"),
            r"[docs/a\_\[1\].md](fuzukue-file:docs/a_%5B1%5D.md)"
        );
    }

    #[test]
    fn file_line_anchors() {
        assert_eq!(parse_file_ref("src/x.rs"), ("src/x.rs", None));
        assert_eq!(parse_file_ref("src/x.rs#L10"), ("src/x.rs", Some(10..=10)));
        assert_eq!(parse_file_ref("src/x.rs#L10-20"), ("src/x.rs", Some(10..=20)));
        assert_eq!(parse_file_ref("src/x.rs#L10-L20"), ("src/x.rs", Some(10..=20)));
        for path in ["x.rs#section", "x.rs#L0", "x.rs#L20-10", "x.rs#L2-", "x.rs#L+2", "#L2"] {
            assert_eq!(parse_file_ref(path), (path, None));
        }
        assert_eq!(
            decode_file_link("docs/a%20%231%25.tex#L2"),
            Some((Cow::Borrowed("docs/a #1%.tex"), Some(2..=2)))
        );
        assert_eq!(decode_file_link(""), None);
        assert_eq!(decode_file_link("%FF"), None);
    }

    #[test]
    fn detects_languages_from_filename_and_shebang() {
        for (path, text, language) in [
            ("script", "#!/usr/bin/env -S python3.12 -u\nprint(1)", "python"),
            ("script.run", "#! /usr/bin/Rscript", "r"),
            ("script", "#!/bin/bash -e", "bash"),
            ("script", "python3", "text"),
            ("script.rs", "#!/usr/bin/python3", "rust"),
            ("GNUmakefile", "", "make"),
            ("dir/CMakeLists.txt", "", "cmake"),
            ("code.C", "", "cpp"),
            ("stats.JL", "", "julia"),
            ("view.tsx", "", "tsx"),
        ] {
            assert_eq!(source_language_with_text(path, text), language);
        }
    }

    #[test]
    fn source_language_by_extension() {
        for (path, language) in [
            ("src/lib.rs", "rust"),
            ("proofs/Main.lean", "lean"),
            ("examples/Main.YU", "yulang"),
            ("README.MD", "markdown"),
            ("Cargo.toml", "toml"),
            ("config.yml", "yaml"),
            ("config.yaml", "yaml"),
            ("paper/main.tex", "tex"),
            ("Makefile", "make"),
            ("docs.v1/file", "text"),
        ] {
            assert_eq!(source_language(path), language);
        }
    }

    #[test]
    fn code_is_untouched() {
        let s = "`[[a]]` と [[b]]\n```\n[[c]]\n```\n[[d]]\n";
        assert_eq!(link_card_refs(s), "`[[a]]` と [b](fuzukue-card:b)\n```\n[[c]]\n```\n[d](fuzukue-card:d)\n");
        let s = "`` ` [[a]]\n[[b]] `` と [[c]]\n\n~~~~sh\n[[d]] $HOME\n~~~~\n[[e]]";
        assert_eq!(link_card_refs(s), "`` ` [[a]]\n[[b]] `` と [c](fuzukue-card:c)\n\n~~~~sh\n[[d]] $HOME\n~~~~\n[e](fuzukue-card:e)");
    }

    #[test]
    fn unclosed_is_kept() {
        assert_eq!(link_card_refs("[[a"), "[[a");
    }

    #[test]
    fn until() {
        assert_eq!(minutes_until(100 + 90, 100), 2);
        assert_eq!(minutes_until(100 + 3720, 100), 62);
        assert_eq!(minutes_until(0, 100), 0);
    }

    #[test]
    fn rank_and_truncate() {
        assert!(kind_rank("goal") < kind_rank("task"));
        assert!(kind_rank("task") < kind_rank("zzz"));
        assert_eq!(truncate("あいうえお", 3), "あいう…");
        assert_eq!(truncate("ab", 3), "ab");
    }

    #[test]
    fn cache_name_flattened() {
        let p = cache_path("a/b\\c:d.pdf");
        assert_eq!(p.file_name().unwrap(), "a_b_c_d.pdf");
    }
}

// ───────────────────────── 出典 ─────────────────────────

/// 出典が指す PDF の位置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceRef {
    /// プロジェクトからの相対パス
    pub pdf: String,
    /// 1 始まりのページ
    pub page: usize,
    /// 原著での番号など（"Theorem 2.4"）
    pub item: String,
}

/// `books/x.pdf#123 Theorem 2.4` / `books/x.pdf#p=123` / `books/x.pdf p.123 Lemma 1` を読む。
/// PDF とページが読めなければ None（論文カードの `draft sections/…:166` などはここに当たる）。
pub fn parse_source(s: &str) -> Option<SourceRef> {
    let s = s.trim();
    let end = s.find(".pdf")? + 4;
    let pdf = s[..end].to_string();
    let rest = s[end..].trim_start();
    let rest = rest.strip_prefix('#').map(|r| r.trim_start_matches("p=").trim_start_matches("page=")).or_else(|| rest.strip_prefix("p."))?;
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    let page: usize = digits.parse().ok().filter(|&p| p > 0)?;
    let item = rest[digits.len()..].trim().to_string();
    Some(SourceRef { pdf, page, item })
}

#[cfg(test)]
mod source_tests {
    use super::*;

    #[test]
    fn forms() {
        let r = parse_source("books/x.pdf#123 Theorem 2.4").unwrap();
        assert_eq!((r.pdf.as_str(), r.page, r.item.as_str()), ("books/x.pdf", 123, "Theorem 2.4"));
        assert_eq!(parse_source("books/x.pdf#p=7").unwrap().page, 7);
        assert_eq!(parse_source("books/x.pdf p.12 Lemma 1").unwrap().item, "Lemma 1");
        assert_eq!(parse_source("draft sections/02-objects.tex:166"), None);
        assert_eq!(parse_source("books/x.pdf"), None);
    }
}

// ───────────────────────── 論文カードの LaTeX を Markdown に寄せる ─────────────────────────

/// カードの本文を `## 見出し` ごとに分ける。見出しの前の部分は見出し "" として返す。
pub fn card_sections(body: &str) -> Vec<(String, String)> {
    card_section_ranges(body)
        .into_iter()
        .map(|(heading, range)| (heading.to_owned(), body[range].to_owned()))
        .collect()
}

pub fn card_section_ranges(body: &str) -> Vec<(&str, Range<usize>)> {
    let mut out = Vec::new();
    let mut heading = "";
    let mut start = 0;
    let mut offset = 0;
    let mut literal_end = None;
    let code = markdown_code::ranges(body);
    let mut fences = code.iter().filter(|code| code.is_fenced()).peekable();
    for line in body.split_inclusive('\n') {
        while fences.peek().is_some_and(|code| code.range.end <= offset) {
            fences.next();
        }
        let in_fence = fences
            .peek()
            .is_some_and(|code| code.range.start < offset + line.len());
        if let Some(end) = literal_end.as_deref() {
            if line.contains(end) {
                literal_end = None;
            }
        } else if !in_fence {
            if let Some(h) = line.strip_prefix("## ") {
                if !heading.is_empty() || !body[start..offset].trim().is_empty() {
                    out.push((heading, start..offset));
                }
                heading = h.trim();
                start = offset + line.len();
            } else if let Some(env) = tex::literal_environment(line) {
                let end = format!("\\end{{{env}}}");
                if !line.contains(&end) {
                    literal_end = Some(end);
                }
            }
        }
        offset += line.len();
    }
    if !heading.is_empty() || !body[start..].trim().is_empty() {
        out.push((heading, start..body.len()));
    }
    out
}

/// `{...}` の中身を取り出す（入れ子の括弧に対応）。`s` は `{` から始まる。返り値は (中身, 残り)。
fn group(s: &str, open: char) -> Option<(&str, &str)> {
    let s = s.strip_prefix(open)?;
    let close = if open == '{' { '}' } else { ']' };
    let mut depth = 0usize;
    let mut chars = s.char_indices();
    while let Some((i, c)) = chars.next() {
        match c {
            '\\' => { chars.next(); }
            c if c == close && depth == 0 => return Some((&s[..i], &s[i + 1..])),
            '{' => depth += 1,
            '}' => depth = depth.checked_sub(1)?,
            _ => {}
        }
    }
    None
}

/// 引数なしのマクロ `\name` を定義に置き換える（後ろが英字のものは別の名前なので触らない）。
pub fn expand_macros(tex: &str, macros: &[(String, String)]) -> String {
    let mut s = tex.to_string();
    for (name, body) in macros {
        let pat = format!("\\{name}");
        let mut out = String::with_capacity(s.len());
        let mut rest = s.as_str();
        while let Some(i) = rest.find(&pat) {
            let after = &rest[i + pat.len()..];
            out.push_str(&rest[..i]);
            if after.chars().next().is_some_and(|c| c.is_ascii_alphabetic()) {
                out.push_str(&pat);
            } else {
                // LaTeX / MathJax と同じく、囲まずにそのまま展開する（\dobs^\rho の添字が縦に並ぶ）。
                out.push_str(body);
            }
            rest = after;
        }
        out.push_str(rest);
        s = out;
    }
    s
}

/// 論文カードの LaTeX 混じりの本文を、Markdown（数式は `$…$` / `$$…$$`）に寄せる。
/// - `\[…\]`、`equation` / `align` / `gather`（* 付きも）は別行の数式にする（align は aligned に）
/// - `\label{…}` は消す
/// - `\emph` / `\textit` は *…*、`\textbf` は **…**
/// - `\Cref` / `\cref` / `\ref` の `card:<id>` はカードへのリンク、それ以外のラベルはコードとして見せる
/// - `\cite[…]{key}` は [key, …]
pub fn tex_to_markdown(body: &str) -> String {
    tex_to_markdown_with_refs(body, None)
}

fn tex_to_markdown_with_refs(body: &str, refs: Option<&fuzukue_proto::DocReferences>) -> String {
    let mut s = body.to_string();
    // 別行の数式
    for (open, close, wrap) in [
        ("\\begin{equation*}", "\\end{equation*}", None),
        ("\\begin{equation}", "\\end{equation}", None),
        ("\\begin{align*}", "\\end{align*}", Some("aligned")),
        ("\\begin{align}", "\\end{align}", Some("aligned")),
        ("\\begin{gather*}", "\\end{gather*}", Some("gathered")),
        ("\\begin{gather}", "\\end{gather}", Some("gathered")),
        ("\\[", "\\]", None),
    ] {
        let mut out = String::with_capacity(s.len());
        let mut rest = s.as_str();
        while let Some(i) = rest.find(open) {
            let Some(j) = rest[i + open.len()..].find(close) else { break };
            let inner = rest[i + open.len()..i + open.len() + j].trim();
            out.push_str(rest[..i].trim_end_matches([' ', '\t']));
            out.push_str("\n\n$$\n");
            match wrap {
                Some(env) => out.push_str(&format!("\\begin{{{env}}}{inner}\\end{{{env}}}")),
                None => out.push_str(inner),
            }
            out.push_str("\n$$\n\n");
            rest = rest[i + open.len() + j + close.len()..].trim_start_matches([' ', '\t']);
        }
        out.push_str(rest);
        s = out;
    }
    // コマンドを順に読んで書き換える
    let mut out = String::with_capacity(s.len());
    let mut rest = s.as_str();
    while let Some(i) = rest.find('\\') {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        let name: String = tail[1..].chars().take_while(|c| c.is_ascii_alphabetic()).collect();
        let after = &tail[1 + name.len()..];
        let handled = match name.as_str() {
            "label" => group(after, '{').map(|(_, r)| (String::new(), r)),
            "emph" | "textit" => group(after, '{').map(|(t, r)| {
                (format!("*{}*", tex_to_markdown_with_refs(t, refs)), r)
            }),
            "textbf" => group(after, '{').map(|(t, r)| {
                (format!("**{}**", tex_to_markdown_with_refs(t, refs)), r)
            }),
            "Cref" | "cref" | "ref" | "eqref" | "autoref" => {
                paper::reference(&name, after, refs)
            }
            "cite" => paper::citation(after, refs),
            _ => None,
        };
        match handled {
            Some((text, r)) => {
                out.push_str(&text);
                rest = r;
            }
            None => {
                // 知らないコマンド（数式の中のものも含む）はそのまま
                out.push('\\');
                out.push_str(&name);
                rest = after;
            }
        }
    }
    out.push_str(rest);
    tex_punctuation(&out)
}

/// TeX の約物: `~`（改行しない空白）は空白に、`---` / `--` はダッシュに。
/// 数式（`$…$`）とインラインコードの中は触らない（数式の `~` は ratex が空白として扱う）。
fn tex_punctuation(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let (mut in_math, mut in_code) = (false, false);
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '`' if !in_math => in_code = !in_code,
            '$' if !in_code && (i == 0 || chars[i - 1] != '\\') => in_math = !in_math,
            _ => {}
        }
        if !in_math && !in_code {
            if c == '~' {
                out.push(' ');
                i += 1;
                continue;
            }
            if c == '-' && chars.get(i + 1) == Some(&'-') {
                if chars.get(i + 2) == Some(&'-') {
                    out.push('—');
                    i += 3;
                } else {
                    out.push('–');
                    i += 2;
                }
                continue;
            }
        }
        out.push(c);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tex_tests {
    use super::*;

    #[test]
    fn sections() {
        let s = card_sections("## Statement\nA\n\n## Proof\nB\n## Notes\nC\n");
        let heads: Vec<_> = s.iter().map(|(h, _)| h.as_str()).collect();
        assert_eq!(heads, vec!["Statement", "Proof", "Notes"]);
        assert_eq!(s[1].1.trim(), "B");
    }

    #[test]
    fn display_math_and_labels() {
        let md = tex_to_markdown("For all $x$,\n\\[\n a\\le b.\n\\]\nand\n\\begin{equation}\n c \\label{core:eq}\n\\end{equation}");
        assert!(md.contains("$$\na\\le b.\n$$"), "{md}");
        assert!(md.contains("$$\nc \n$$") || md.contains("$$\nc\n$$"), "{md}");
        assert!(!md.contains("label"), "{md}");
    }

    #[test]
    fn align_becomes_aligned() {
        let md = tex_to_markdown("\\begin{align*} a&=b\\\\ c&=d \\end{align*}");
        assert!(md.contains("\\begin{aligned}a&=b\\\\ c&=d\\end{aligned}"), "{md}");
    }

    #[test]
    fn refs_cites_and_emphasis() {
        let md = tex_to_markdown("\\emph{集中する}、\\Cref{card:a, mm:thm-x}、\\cite[Thm~5.1]{Bill1999}");
        assert_eq!(md, "*集中する*、[a](fuzukue-card:a), `mm:thm-x`、[Bill1999, Thm 5.1]");
    }

    #[test]
    fn tex_punctuation_outside_math_and_code() {
        assert_eq!(
            tex_to_markdown("Propositions~4.3 and~5.4, Ornstein--Uhlenbeck --- $a~b$ `x--y`"),
            "Propositions 4.3 and 5.4, Ornstein–Uhlenbeck — $a~b$ `x--y`"
        );
    }

    #[test]
    fn macros_expand_only_whole_names() {
        let m = vec![("dobs".to_string(), "d_{\\mathrm{obs}}".to_string())];
        assert_eq!(expand_macros("$\\dobs^\\rho(X)$ $\\dobsx$", &m), "$d_{\\mathrm{obs}}^\\rho(X)$ $\\dobsx$");
    }
}
