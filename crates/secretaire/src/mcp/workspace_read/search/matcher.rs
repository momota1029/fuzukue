use anyhow::{ensure, Context, Result};
use regex_automata::{meta::Regex, util::syntax};
use std::time::Instant;

use super::{Args, OutputMode};

pub(super) const MAX_PATTERNS: usize = 16;
pub(super) const MAX_PATTERN_BYTES: usize = 4096;

pub(super) struct Matcher {
    regex: Regex,
    multiline: bool,
    invert: bool,
    only_matching: bool,
}

pub(super) struct FileText<'a> {
    text: &'a str,
    pub lines: Vec<&'a str>,
}

impl<'a> FileText<'a> {
    pub(super) fn new(text: &'a str) -> Self {
        Self {
            text,
            lines: text.lines().collect(),
        }
    }

    fn line_start(&self, line: usize) -> usize {
        self.lines[line].as_ptr().addr() - self.text.as_ptr().addr()
    }

    fn line_at(&self, byte: usize) -> usize {
        // Offset lookup needs no extra line table, even for files with many empty lines.
        let mut lo = 0;
        let mut hi = self.lines.len();
        while lo + 1 < hi {
            let mid = (lo + hi) / 2;
            if self.line_start(mid) <= byte {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        lo
    }
}

pub(super) struct Hit<'a> {
    pub first: usize,
    pub last: usize,
    pub column: Option<usize>,
    pub text: Option<&'a str>,
}

impl Hit<'_> {
    fn inverted(line: usize) -> Self {
        Self {
            first: line,
            last: line,
            column: None,
            text: None,
        }
    }
}

impl Matcher {
    pub(super) fn new(args: &Args) -> Result<Self> {
        ensure!(
            args.patterns.len() <= MAX_PATTERNS,
            "patterns accepts at most {MAX_PATTERNS} entries"
        );
        let mut parameters = Vec::new();
        if let Some(pattern) = &args.pattern {
            parameters.push(("pattern".into(), pattern));
        }
        parameters.extend(
            args.patterns
                .iter()
                .enumerate()
                .map(|(i, pattern)| (format!("patterns[{i}]"), pattern)),
        );
        ensure!(
            !parameters.is_empty(),
            "Supply pattern or patterns, or use output_mode=files to list files"
        );

        let mut patterns = Vec::new();
        for (parameter, pattern) in &parameters {
            ensure!(
                pattern.len() <= MAX_PATTERN_BYTES && !pattern.contains('\0'),
                "{parameter} must be at most {MAX_PATTERN_BYTES} bytes and contain no NUL"
            );
            ensure!(
                args.multiline || !pattern.contains(['\n', '\r']),
                "{parameter} contains a line break. Use multiline=true for one pattern spanning lines, or patterns for separate alternatives"
            );
            let mut pattern = if args.fixed_string {
                regex::escape(pattern)
            } else {
                (*pattern).clone()
            };
            if args.word {
                pattern = format!(r"\b{{start-half}}(?:{pattern})\b{{end-half}}");
            }
            if args.whole_line {
                pattern = if args.multiline {
                    format!("(?m:^(?:{pattern})$)")
                } else {
                    format!(r"\A(?:{pattern})\z")
                };
            }
            patterns.push(pattern);
        }
        let ignore_case = args.ignore_case
            || args.smart_case
                && parameters
                    .iter()
                    .all(|(_, pattern)| !pattern.chars().any(char::is_uppercase));
        let regex = Regex::builder()
            .configure(Regex::config().nfa_size_limit(Some(2 * 1024 * 1024)))
            .syntax(syntax::Config::new().case_insensitive(ignore_case).crlf(true))
            .build_many(&patterns)
            .map_err(|error| {
                let parameter = error.pattern().map_or("pattern(s)", |id| parameters[id.as_usize()].0.as_str());
                let cause = std::error::Error::source(&error);
                let message = cause.map_or_else(|| error.to_string(), ToString::to_string);
                anyhow::anyhow!(
                    "Invalid {parameter}: {message}. Use fixed_string=true for literal text. For grep -P lookaround or backreferences, use a regular expression that consumes the surrounding text, context, or separate searches"
                )
            })
            .context("Could not compile the search expression")?;
        Ok(Self {
            regex,
            multiline: args.multiline,
            invert: args.invert,
            only_matching: args.only_matching && args.output_mode == OutputMode::Content,
        })
    }

    // False means the deadline interrupted the file. The callback can end a completed
    // search early when it has enough information for the requested output mode.
    pub(super) fn visit<'a>(
        &self,
        file: &FileText<'a>,
        deadline: Instant,
        mut emit: impl FnMut(Hit<'a>) -> bool,
    ) -> bool {
        if self.multiline {
            return self.visit_multiline(file, deadline, emit);
        }
        for (index, line) in file.lines.iter().enumerate() {
            if Instant::now() >= deadline {
                return false;
            }
            if self.invert {
                if !self.regex.is_match(line) && !emit(Hit::inverted(index)) {
                    return true;
                }
            } else {
                for found in self.regex.find_iter(line) {
                    if Instant::now() >= deadline {
                        return false;
                    }
                    let hit = Hit {
                        first: index,
                        last: index,
                        column: Some(found.start() + 1),
                        text: Some(&line[found.range()]),
                    };
                    if !emit(hit) {
                        return true;
                    }
                    if !self.only_matching {
                        break;
                    }
                }
            }
        }
        true
    }

    fn visit_multiline<'a>(
        &self,
        file: &FileText<'a>,
        deadline: Instant,
        mut emit: impl FnMut(Hit<'a>) -> bool,
    ) -> bool {
        if file.lines.is_empty() {
            return true;
        }
        let mut pending: Option<Hit<'a>> = None;
        let mut next_unmatched = 0;
        for found in self.regex.find_iter(file.text) {
            if Instant::now() >= deadline {
                return false;
            }
            // A trailing newline terminates the last line without creating a new one.
            if found.start() == file.text.len() && file.text.ends_with('\n') {
                break;
            }
            let first = file.line_at(found.start());
            let last = file.line_at(found.end().saturating_sub(1).max(found.start()));
            if self.invert {
                for line in next_unmatched..first {
                    if Instant::now() >= deadline {
                        return false;
                    }
                    if !emit(Hit::inverted(line)) {
                        return true;
                    }
                }
                next_unmatched = next_unmatched.max(last + 1);
                continue;
            }
            let hit = Hit {
                first,
                last,
                column: Some(found.start() - file.line_start(first) + 1),
                text: Some(&file.text[found.range()]),
            };
            if self.only_matching {
                if !emit(hit) {
                    return true;
                }
            } else if let Some(previous) =
                pending.as_mut().filter(|previous| first <= previous.last)
            {
                previous.last = previous.last.max(last);
            } else if pending.replace(hit).is_some_and(|previous| !emit(previous)) {
                return true;
            }
        }
        if let Some(hit) = pending {
            emit(hit);
        }
        if self.invert {
            for line in next_unmatched..file.lines.len() {
                if Instant::now() >= deadline {
                    return false;
                }
                if !emit(Hit::inverted(line)) {
                    return true;
                }
            }
        }
        Instant::now() < deadline
    }
}
