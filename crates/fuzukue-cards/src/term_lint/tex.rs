//! A small TeX scanner for prose. It does not expand macros.

use std::ops::Range;

#[derive(Clone, Copy)]
enum Math<'a> {
    Dollar,
    DoubleDollar,
    Command(&'static str),
    Environment(&'a str),
}

impl Math<'_> {
    fn closes_with_dollars(self, count: usize) -> bool {
        matches!((self, count), (Self::Dollar, _) | (Self::DoubleDollar, 2))
    }
}

pub(super) fn prose(input: &str, emphasis: impl FnMut(&str)) -> String {
    scan(input, emphasis, false, true, |_, _| {})
}

pub(crate) fn style_prose(input: &str, source: impl FnMut(Range<usize>, usize)) -> String {
    scan(input, |_| {}, true, true, source)
}

/// Keep prose characters at their original byte offsets. Math, comments and
/// TeX syntax are replaced with spaces so exact matches can address the file.
pub(crate) fn prose_mask(input: &str, tex_comments: bool) -> String {
    let mut masked: Vec<_> = input.bytes().map(|byte| if matches!(byte, b'\n' | b'\r') { byte } else { b' ' }).collect();
    scan(input, |_| {}, true, tex_comments, |output, start| {
        let c = input[start..].chars().next().unwrap();
        if output.len() == c.len_utf8() && !matches!(c, '\0' | '\\' | '$' | '{' | '}')
            && !(tex_comments && c == '%') && !input[start..].starts_with("<!--")
        {
            let end = start + c.len_utf8();
            masked[start..end].copy_from_slice(&input.as_bytes()[start..end]);
        }
    });
    String::from_utf8(masked).unwrap()
}

fn scan(
    mut input: &str,
    mut emphasis: impl FnMut(&str),
    style: bool,
    tex_comments: bool,
    mut source: impl FnMut(Range<usize>, usize),
) -> String {
    let mut output = String::with_capacity(input.len());
    let input_len = input.len();
    let mut previous = (0, 0);
    let mut math = None;
    loop {
        // Attribute any output from the previous step to the source it consumed.
        if previous.1 < output.len() {
            source(previous.1..output.len(), previous.0);
        }
        let Some(first) = input.chars().next() else {
            break;
        };
        previous = (input_len - input.len(), output.len());
        if style && math.is_none() {
            if let Some(comment) = input.strip_prefix("<!--") {
                input = comment.split_once("-->").map_or("", |(_, rest)| rest);
                output.push('\0');
                continue;
            }
        }
        match first {
            '\0' => {
                if !style {
                    math = None;
                }
                if math.is_none() {
                    output.push('\0');
                }
                input = &input[1..];
            }
            '%' if tex_comments || math.is_some() => {
                let keep_line = style && math.is_none() && input.contains('\n');
                input = after_comment(input);
                if !style || math.is_none() {
                    output.push('\0');
                }
                if keep_line {
                    output.push('\n');
                }
            }
            '$' => {
                let count = if input.starts_with("$$") && !matches!(math, Some(Math::Dollar)) {
                    2
                } else {
                    1
                };
                match math {
                    None => {
                        math = Some(if count == 2 {
                            Math::DoubleDollar
                        } else {
                            Math::Dollar
                        });
                        output.push('\0');
                    }
                    Some(mode) if mode.closes_with_dollars(count) => math = None,
                    _ => {}
                }
                input = &input[count..];
            }
            '\\' => {
                let (name, rest) = command(&input[1..]);
                input = rest;
                if let Some(mode) = math {
                    match mode {
                        Math::Command(end) if name == end => math = None,
                        Math::Environment(env) if name == "end" => {
                            if let Some((group, rest)) = argument(input, b'{', b'}') {
                                if group == env {
                                    math = None;
                                    input = rest;
                                }
                            }
                        }
                        _ => {}
                    }
                    continue;
                }
                match name {
                    "(" | "[" => {
                        math = Some(Math::Command(if name == "(" { ")" } else { "]" }));
                        output.push('\0');
                    }
                    "begin" | "end" => {
                        if let Some((env, rest)) = argument(input, b'{', b'}') {
                            if name == "begin" && is_math_environment(env) {
                                math = Some(Math::Environment(env));
                            }
                            input = rest;
                            if name == "begin" && is_opaque_environment(env) {
                                let close = format!("\\end{{{env}}}");
                                input = input.split_once(&close).map_or("", |(_, rest)| rest);
                                output.push_str(if style { "\n\n" } else { "\0" });
                                continue;
                            }
                        }
                        output.push('\0');
                    }
                    "verb" => {
                        let content = input.strip_prefix('*').unwrap_or(input);
                        if let Some(delimiter) =
                            content.chars().next().filter(|c| !c.is_whitespace())
                        {
                            let content = &content[delimiter.len_utf8()..];
                            let end = content.find(['\n', delimiter]).unwrap_or(content.len());
                            input = content[end..]
                                .strip_prefix(delimiter)
                                .unwrap_or(&content[end..]);
                        }
                        output.push('\0');
                    }
                    "emph" | "textbf" => {
                        if let Some((group, _)) = argument(input, b'{', b'}') {
                            emphasis(group);
                        }
                        if !style {
                            output.push('\0');
                        }
                    }
                    name if is_reference(name) => {
                        input = input.strip_prefix('*').unwrap_or(input);
                        while let Some((_, rest)) = argument(input, b'[', b']') {
                            input = rest;
                        }
                        if let Some((_, rest)) = argument(input, b'{', b'}') {
                            input = rest;
                        }
                        output.push('\0');
                    }
                    "%" | "$" | "{" | "}" | "_" | "&" | "#" | " " => output.push_str(name),
                    "textit" | "textnormal" | "textrm" | "mbox" if style => {}
                    "par" | "item" if style => {
                        if name == "item" {
                            if let Some((_, rest)) = argument(input, b'[', b']') {
                                input = rest;
                            }
                        }
                        output.push_str("\n\n");
                    }
                    _ => output.push('\0'),
                }
            }
            '{' | '}' if style && math.is_none() => input = &input[1..],
            '~' if math.is_none() => {
                output.push(' ');
                input = &input[1..];
            }
            _ => {
                if math.is_none() {
                    output.push(first);
                }
                input = &input[first.len_utf8()..];
            }
        }
    }
    output
}

fn is_opaque_environment(name: &str) -> bool {
    matches!(
        name,
        "verbatim" | "verbatim*" | "Verbatim" | "lstlisting" | "minted" | "comment"
    )
}

fn command(input: &str) -> (&str, &str) {
    let letters = input.bytes().take_while(u8::is_ascii_alphabetic).count();
    let len = if letters > 0 {
        letters
    } else {
        input.chars().next().map_or(0, char::len_utf8)
    };
    input.split_at(len)
}

fn after_comment(input: &str) -> &str {
    input.find('\n').map_or("", |end| &input[end + 1..])
}

fn argument(mut input: &str, open: u8, close: u8) -> Option<(&str, &str)> {
    loop {
        input = input.trim_start();
        if !input.starts_with('%') {
            break;
        }
        input = after_comment(input);
    }
    if input.as_bytes().first().copied()? != open {
        return None;
    }
    let start = &input[1..];
    let mut rest = start;
    let mut depth = 1;
    while let Some(c) = rest.as_bytes().first().copied() {
        match c {
            b'\\' => rest = command(&rest[1..]).1,
            b'%' => rest = after_comment(rest),
            b'{' if open == b'[' => rest = argument(rest, b'{', b'}')?.1,
            c if c == open => {
                depth += 1;
                rest = &rest[1..];
            }
            c if c == close => {
                depth -= 1;
                if depth == 0 {
                    return Some((&start[..start.len() - rest.len()], &rest[1..]));
                }
                rest = &rest[1..];
            }
            _ => rest = &rest[rest.chars().next()?.len_utf8()..],
        }
    }
    None
}

fn is_reference(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    name.starts_with("cite")
        || matches!(
            name.as_str(),
            "label"
                | "ref"
                | "cref"
                | "eqref"
                | "pageref"
                | "autoref"
                | "nameref"
                | "parencite"
                | "textcite"
                | "autocite"
                | "footcite"
                | "nocite"
        )
}

fn is_math_environment(name: &str) -> bool {
    matches!(
        name.trim_end_matches('*'),
        "math"
            | "displaymath"
            | "equation"
            | "align"
            | "alignat"
            | "flalign"
            | "gather"
            | "multline"
            | "split"
            | "aligned"
            | "alignedat"
            | "gathered"
            | "eqnarray"
            | "array"
            | "cases"
            | "matrix"
            | "pmatrix"
            | "bmatrix"
            | "Bmatrix"
            | "vmatrix"
            | "Vmatrix"
            | "smallmatrix"
            | "subequations"
    )
}
