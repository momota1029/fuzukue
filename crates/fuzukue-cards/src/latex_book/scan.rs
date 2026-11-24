//! A lexical scanner for literal LaTeX structure. TeX macro expansion is left
//! to the original and imported PDF builds.
use std::ops::Range;
use std::sync::LazyLock;

use regex::Regex;

static TOKEN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\\(?:[a-zA-Z@]+|[^\n])|%[^\n]*|[{}]|\r?\n(?:[ \t]*\r?\n)+").unwrap()
});

pub(super) fn tokens(tex: &str) -> impl Iterator<Item = Range<usize>> + '_ {
    let mut offset = 0;
    std::iter::from_fn(move || loop {
        let m = TOKEN.find_at(tex, offset)?;
        offset = m.end();
        match m.as_str() {
            "\\verb" => {
                let tail = tex[offset..].strip_prefix('*').unwrap_or(&tex[offset..]);
                if let Some(delimiter) = tail.chars().next().filter(|c| !c.is_whitespace()) {
                    if let Some(end) = tail[delimiter.len_utf8()..].find(delimiter) {
                        offset = tex.len() - tail.len() + end + 2 * delimiter.len_utf8();
                    }
                }
            }
            "\\begin" => {
                let (name, end) = argument(tex, offset, '{', '}').unwrap_or(("", offset));
                if matches!(
                    name,
                    "verbatim" | "verbatim*" | "Verbatim" | "lstlisting" | "minted" | "comment"
                ) {
                    let close = format!("\\end{{{name}}}");
                    offset = tex[end..]
                        .find(&close)
                        .map_or(tex.len(), |pos| end + pos + close.len());
                } else {
                    return Some(m.range());
                }
            }
            _ if m.as_str().starts_with('%') => {}
            _ => return Some(m.range()),
        }
    })
}

pub(super) fn whitespace(tex: &str, mut offset: usize) -> usize {
    loop {
        let tail = tex[offset..].trim_start();
        offset = tex.len() - tail.len();
        if !tail.starts_with('%') {
            return offset;
        }
        offset += tail.find('\n').unwrap_or(tail.len());
    }
}

pub(super) fn argument(tex: &str, offset: usize, open: char, close: char) -> Option<(&str, usize)> {
    let start = whitespace(tex, offset);
    let tail = tex[start..].strip_prefix(open)?;
    let mut depth = 1;
    let mut braces = 0;
    let mut chars = tail.char_indices();
    while let Some((i, c)) = chars.next() {
        match c {
            '\\' => {
                chars.next();
            }
            '%' => {
                chars
                    .by_ref()
                    .take_while(|&(_, c)| c != '\n')
                    .for_each(drop);
            }
            '{' if open != '{' => braces += 1,
            '}' if open != '{' => braces -= 1,
            c if c == open && braces == 0 => depth += 1,
            c if c == close && braces == 0 => {
                depth -= 1;
                if depth == 0 {
                    return Some((&tail[..i], start + i + 2));
                }
            }
            _ => {}
        }
    }
    None
}

pub(super) fn starred(tex: &str, offset: usize) -> usize {
    let next = whitespace(tex, offset);
    if tex[next..].starts_with('*') {
        next + 1
    } else {
        offset
    }
}

pub(super) fn optional(tex: &str, offset: usize) -> usize {
    argument(tex, offset, '[', ']').map_or(offset, |(_, end)| end)
}

pub(super) fn document(tex: &str) -> Option<(Range<usize>, Range<usize>)> {
    let mut begin = None;
    for token in tokens(tex) {
        let command = &tex[token.clone()];
        if matches!(command, "\\begin" | "\\end") {
            if let Some(("document", end)) = argument(tex, token.end, '{', '}') {
                let range = token.start..end;
                if command == "\\begin" {
                    begin = Some(range);
                } else if let Some(begin) = begin {
                    return Some((begin, range));
                }
            }
        }
    }
    None
}
