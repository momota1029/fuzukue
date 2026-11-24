//! Shared preflight checks for a standalone card TeX fragment.

use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Access {
    #[default]
    Unavailable,
    Editable,
    ImportedManuscript,
    ManuscriptBuild,
}

impl Access {
    pub fn is_unavailable(&self) -> bool {
        *self == Self::Unavailable
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IssueKind {
    NullByte,
    UnbalancedGroup,
    UnbalancedEnvironment,
    UnterminatedVerb,
    DocumentCommand,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Issue {
    /// One-based line in the submitted fragment.
    pub line: usize,
    pub kind: IssueKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum BuildOutcome {
    Built {
        pdf: String,
    },
    Failed {
        log: String,
        log_path: Option<String>,
    },
    Superseded,
}

/// Check literal groups, environments and document commands. TeX expansion,
/// math syntax and deck-specific macros are checked by the actual build.
pub fn validate(source: &str) -> Result<(), Issue> {
    use IssueKind::*;

    if let Some(index) = source.find('\0') {
        return Err(Issue {
            line: 1 + newlines(&source[..index]),
            kind: NullByte,
        });
    }
    let mut groups = Vec::new();
    let mut environments = Vec::new();
    let mut line = 1;
    let mut rest = source;
    while let Some(ch) = rest.chars().next() {
        let issue = |kind| Issue { line, kind };
        rest = &rest[ch.len_utf8()..];
        match ch {
            '\n' => line += 1,
            '%' => rest = after_comment(rest),
            '{' => groups.push(line),
            '}' if groups.pop().is_none() => return Err(issue(UnbalancedGroup)),
            '\\' => {
                let len = rest.bytes().take_while(u8::is_ascii_alphabetic).count();
                if len == 0 {
                    if let Some(escaped) = rest.chars().next() {
                        line += usize::from(escaped == '\n');
                        rest = &rest[escaped.len_utf8()..];
                    }
                    continue;
                }
                let (command, tail) = rest.split_at(len);
                rest = tail;
                match command {
                    "verb" => {
                        let literal = rest.strip_prefix('*').unwrap_or(rest);
                        let first_line = literal.split(['\r', '\n']).next().unwrap();
                        let delimiter = first_line
                            .chars()
                            .next()
                            .ok_or_else(|| issue(UnterminatedVerb))?;
                        let body = &first_line[delimiter.len_utf8()..];
                        let end = body
                            .find(delimiter)
                            .ok_or_else(|| issue(UnterminatedVerb))?;
                        rest = &literal[end + 2 * delimiter.len_utf8()..];
                    }
                    "begin" | "end" => {
                        let Some((name, tail)) = environment(rest) else {
                            continue;
                        };
                        if name == "document" {
                            return Err(issue(DocumentCommand));
                        }
                        let command_line = line;
                        line += newlines(&rest[..rest.len() - tail.len()]);
                        rest = tail;
                        if command == "end" {
                            if !environments.pop().is_some_and(|(open, _)| open == name) {
                                return Err(Issue {
                                    line: command_line,
                                    kind: UnbalancedEnvironment,
                                });
                            }
                        } else if matches!(
                            name,
                            "verbatim"
                                | "verbatim*"
                                | "Verbatim"
                                | "lstlisting"
                                | "minted"
                                | "comment"
                        ) {
                            let end = format!(r"\end{{{name}}}");
                            let Some((body, tail)) = rest.split_once(&end) else {
                                return Err(Issue {
                                    line: command_line,
                                    kind: UnbalancedEnvironment,
                                });
                            };
                            line += newlines(body);
                            rest = tail;
                        } else {
                            environments.push((name, command_line));
                        }
                    }
                    "documentclass" | "usepackage" | "RequirePackage" | "input" | "include"
                    | "includeonly" | "frontmatter" | "mainmatter" | "backmatter" | "part"
                    | "chapter" | "section" | "subsection" | "subsubsection" | "bibliography"
                    | "addbibresource" | "endinput" => return Err(issue(DocumentCommand)),
                    _ => {}
                }
            }
            _ => {}
        }
    }
    if let Some((_, line)) = environments.pop() {
        return Err(Issue {
            line,
            kind: UnbalancedEnvironment,
        });
    }
    if let Some(line) = groups.pop() {
        return Err(Issue {
            line,
            kind: UnbalancedGroup,
        });
    }
    Ok(())
}

fn newlines(text: &str) -> usize {
    text.bytes().filter(|&byte| byte == b'\n').count()
}

fn after_comment(text: &str) -> &str {
    text.find('\n').map_or("", |index| &text[index..])
}

fn environment(mut tail: &str) -> Option<(&str, &str)> {
    loop {
        tail = tail.trim_start();
        match tail.strip_prefix('%') {
            Some(comment) => tail = after_comment(comment),
            None => break,
        }
    }
    let (name, rest) = tail.strip_prefix('{')?.split_once('}')?;
    // A macro can supply an environment name. The build resolves these.
    (!name.is_empty() && !name.contains(['\\', '%', '{']) && !name.contains(char::is_whitespace))
        .then_some((name, rest))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checks_literal_structure_and_skips_comments_and_verbatim() {
        use IssueKind::*;
        assert_eq!(
            validate(concat!(
                "% \\documentclass{bad}\r\n",
                "\\begin{align*} x &= \\{y\\} \\\\ \\end{align*}\r\n",
                "\\verb|{%}| \\% text\n",
                "\\begin % comment\n{align} x = y \\end\n{align}\n",
                "\\begin{verbatim}\n\\end{document}\n\\end{verbatim}\n",
            )),
            Ok(())
        );
        for (source, kind, line) in [
            ("text\n{\\frac{1}{2}", UnbalancedGroup, 2),
            (
                "text\n\\begin{align}\\end{gather}",
                UnbalancedEnvironment,
                2,
            ),
            ("text\n\\section{Heading}", DocumentCommand, 2),
            ("text\n\\begin {document}", DocumentCommand, 2),
            ("text\n\\begin% comment\n{document}", DocumentCommand, 2),
            ("\\verb|x", UnterminatedVerb, 1),
        ] {
            assert_eq!(validate(source), Err(Issue { line, kind }));
        }
    }
}
