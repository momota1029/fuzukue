use std::collections::BTreeMap;

use anyhow::{ensure, Context, Result};

pub(super) struct Entry {
    pub(super) kind: String,
    pub(super) fields: BTreeMap<String, String>,
}

pub(super) fn parse(input: &str) -> Result<BTreeMap<String, Entry>> {
    let mut parser = Parser {
        rest: input,
        strings: [
            "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
        ]
        .into_iter()
        .map(|month| (month.to_owned(), month.to_owned()))
        .collect(),
    };
    let mut entries = BTreeMap::new();
    while let Some(next) = parser.rest.find(['@', '%']) {
        parser.rest = &parser.rest[next..];
        if parser.rest.starts_with('%') {
            parser.skip_space();
            continue;
        }
        parser.rest = &parser.rest[1..];
        parser.entry(&mut entries).with_context(|| {
            let offset = input.len() - parser.rest.len();
            let line = 1 + input[..offset].bytes().filter(|&b| b == b'\n').count();
            format!("line {line}")
        })?;
    }
    Ok(entries)
}

struct Parser<'a> {
    rest: &'a str,
    strings: BTreeMap<String, String>,
}

impl<'a> Parser<'a> {
    fn entry(&mut self, entries: &mut BTreeMap<String, Entry>) -> Result<()> {
        let kind = self.token()?.to_ascii_lowercase();
        let end = if self.eat('{') {
            Some('}')
        } else if self.eat('(') {
            Some(')')
        } else {
            None
        };
        if kind == "comment" {
            if let Some(end) = end {
                self.literal(end)?;
            }
            return Ok(());
        }
        let end = end.context("expected { or ( after BibTeX entry type")?;

        match kind.as_str() {
            "preamble" => {
                self.value()?;
                self.eat(',');
                self.expect(end)?;
            }
            "string" => {
                let (name, value) = self.field()?;
                self.eat(',');
                self.expect(end)?;
                self.strings.insert(name, value);
            }
            _ => {
                let key = self.token()?.to_owned();
                ensure!(!entries.contains_key(&key), "duplicate BibTeX key: {key}");
                let mut fields = BTreeMap::new();
                while !self.eat(end) {
                    self.expect(',')?;
                    if self.eat(end) {
                        break;
                    }
                    let (name, value) = self.field()?;
                    // BibTeX uses the first occurrence of a repeated field.
                    fields.entry(name).or_insert(value);
                }
                entries.insert(key, Entry { kind, fields });
            }
        }
        Ok(())
    }

    fn field(&mut self) -> Result<(String, String)> {
        let name = self.token()?.to_ascii_lowercase();
        self.expect('=')?;
        Ok((name, self.value()?))
    }

    fn value(&mut self) -> Result<String> {
        let mut value = String::new();
        loop {
            self.skip_space();
            if let Some(rest) = self.rest.strip_prefix('{') {
                self.rest = rest;
                value.push_str(self.literal('}')?);
            } else if let Some(rest) = self.rest.strip_prefix('"') {
                self.rest = rest;
                value.push_str(self.literal('"')?);
            } else {
                let token = self.token()?;
                if token.bytes().all(|b| b.is_ascii_digit()) {
                    value.push_str(token);
                } else if let Some(expanded) = self.strings.get(&token.to_ascii_lowercase()) {
                    value.push_str(expanded);
                }
            }
            if !self.eat('#') {
                return Ok(value);
            }
        }
    }

    fn literal(&mut self, end: char) -> Result<&'a str> {
        let mut braces = 0;
        let mut parens = 0;
        let mut chars = self.rest.char_indices();
        while let Some((index, ch)) = chars.next() {
            match ch {
                ch if ch == end && braces == 0 && parens == 0 => {
                    let value = &self.rest[..index];
                    self.rest = chars.as_str();
                    return Ok(value);
                }
                '\\' => {
                    chars.next();
                }
                '{' => braces += 1,
                '}' if braces > 0 => braces -= 1,
                '}' if end == '"' => anyhow::bail!("unbalanced brace in quoted BibTeX value"),
                '(' if end == ')' && braces == 0 => parens += 1,
                ')' if end == ')' && braces == 0 => parens -= 1,
                _ => {}
            }
        }
        anyhow::bail!("missing closing {end}")
    }

    fn token(&mut self) -> Result<&'a str> {
        self.skip_space();
        let len = self
            .rest
            .find(|ch: char| {
                ch.is_whitespace()
                    || matches!(
                        ch,
                        '"' | '#' | '%' | '\'' | '(' | ')' | ',' | '=' | '{' | '}'
                    )
            })
            .unwrap_or(self.rest.len());
        ensure!(len > 0, "expected BibTeX identifier");
        let (token, remaining) = self.rest.split_at(len);
        self.rest = remaining;
        Ok(token)
    }

    fn skip_space(&mut self) {
        self.rest = self.rest.trim_start();
        while self.rest.starts_with('%') {
            self.rest = self
                .rest
                .split_once('\n')
                .map_or("", |(_, rest)| rest.trim_start());
        }
    }

    fn eat(&mut self, ch: char) -> bool {
        self.skip_space();
        if let Some(rest) = self.rest.strip_prefix(ch) {
            self.rest = rest;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, ch: char) -> Result<()> {
        ensure!(self.eat(ch), "expected {ch}");
        Ok(())
    }
}
