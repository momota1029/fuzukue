//! Bash command lines read through the tree-sitter grammar into the simple
//! commands they contain. Whatever cannot be read faithfully makes the whole
//! line unreadable, so the caller asks a person.

use tree_sitter::Node;

/// Stands for the part of a word that is only known when the shell runs.
pub(super) const UNKNOWN: char = '\0';
const LIMIT: usize = 256 * 1024;

/// A word after quote removal.
#[derive(Clone, Debug, Default)]
pub(super) struct Word {
    /// Literal text, with [`UNKNOWN`] in place of every expansion.
    pub text: String,
    /// The shell decides part of the value: a variable, substitution or brace list.
    pub dynamic: bool,
    /// An unquoted pattern that the shell may replace with file names.
    pub glob: bool,
}

impl Word {
    pub(super) fn literal(text: &str) -> Self {
        Self { text: text.into(), ..Self::default() }
    }

    pub(super) fn unknown() -> Self {
        Self { text: UNKNOWN.into(), dynamic: true, glob: false }
    }
}

/// One program invocation.
#[derive(Debug, Default)]
pub(super) struct Simple {
    pub assignments: Vec<Word>,
    pub words: Vec<Word>,
    /// Standard input, when a here-document or here-string spells it out.
    pub stdin: Option<String>,
}

#[derive(Debug, Default)]
pub(super) struct Script {
    pub commands: Vec<Simple>,
    /// Every word of the line, for path checks.
    pub words: Vec<Word>,
}

/// Redirections that belong to a command.
#[derive(Default)]
struct Attached {
    /// Words the grammar placed in a redirect although the shell passes them on.
    words: Vec<(usize, Word)>,
    /// `Some(None)` is input that cannot be read here.
    stdin: Option<Option<String>>,
}

impl Attached {
    fn input(&mut self, text: Option<String>) {
        self.stdin = Some(if self.stdin.is_some() { None } else { text });
    }
}

pub(super) fn tree(text: &str) -> Option<tree_sitter::Tree> {
    if text.len() > LIMIT || text.contains(UNKNOWN) || joins_words(text) {
        return None;
    }
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&tree_sitter_bash::LANGUAGE.into()).ok()?;
    let tree = parser.parse(text, None)?;
    (!tree.root_node().has_error()).then_some(tree)
}

/// A backslash-newline inside a word, which the grammar reads as a word break.
fn joins_words(text: &str) -> bool {
    ["\\\n", "\\\r\n"].iter().any(|continuation| text.match_indices(continuation).any(|(index, found)| {
        let before = text[..index].chars().next_back();
        let after = text[index + found.len()..].chars().next();
        before.is_some_and(|c| !c.is_whitespace()) && after.is_some_and(|c| !c.is_whitespace())
    }))
}

pub(super) fn children(node: Node<'_>) -> Vec<(Option<&'static str>, Node<'_>)> {
    let mut cursor = node.walk();
    let mut found = Vec::new();
    if cursor.goto_first_child() {
        loop {
            found.push((cursor.field_name(), cursor.node()));
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
    found
}

/// Every node, parents before children and in source order.
pub(super) fn nodes(root: Node<'_>) -> Vec<Node<'_>> {
    let mut found = Vec::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        found.push(node);
        stack.extend(children(node).into_iter().rev().map(|(_, child)| child));
    }
    found
}

pub(super) fn script(text: &str, home: Option<&str>) -> Option<Script> {
    let tree = tree(text)?;
    let reader = Reader { text, home };
    let all = nodes(tree.root_node());
    let mut attached = std::collections::HashMap::<usize, Attached>::new();
    let mut script = Script::default();
    // Redirections come first, since they can follow the command they belong to.
    for node in all.iter().filter(|node| node.kind() == "redirected_statement") {
        let parts = children(*node);
        let body = parts.iter().find(|(field, _)| *field == Some("body")).map(|(_, body)| *body);
        let mut found = Attached::default();
        for (field, child) in &parts {
            if *field != Some("body") {
                reader.redirect(*child, &mut found)?;
            }
        }
        match body.and_then(target) {
            Some(command) => {
                let entry = attached.entry(command.id()).or_default();
                entry.words.append(&mut found.words);
                if let Some(input) = found.stdin {
                    entry.input(input);
                }
            }
            None if found.words.is_empty() => {}
            // Words after a group or a loop have no command to join.
            None if body.is_some() => return None,
            None => {
                found.words.sort_by_key(|(start, _)| *start);
                script.commands.push(Simple {
                    words: found.words.into_iter().map(|(_, word)| word).collect(),
                    stdin: found.stdin.flatten(),
                    ..Simple::default()
                });
            }
        }
    }
    for node in &all {
        match node.kind() {
            "command" => {
                let mut own = attached.remove(&node.id()).unwrap_or_default();
                let mut assignments = Vec::new();
                for (field, child) in children(*node) {
                    match (field, child.kind()) {
                        (_, "variable_assignment") => assignments.push(reader.assignment(child)),
                        (_, "comment") => {}
                        (_, "file_redirect" | "herestring_redirect") => reader.redirect(child, &mut own)?,
                        (Some("name"), _) => {
                            let inner = child.named_child(0).filter(|_| child.named_child_count() == 1);
                            own.words.push((child.start_byte(), reader.word(inner.unwrap_or(child))));
                        }
                        (Some("argument"), _) => own.words.push((child.start_byte(), reader.word(child))),
                        // `name ( … )` and anything newer than this reader.
                        _ => return None,
                    }
                }
                own.words.sort_by_key(|(start, _)| *start);
                script.commands.push(Simple {
                    assignments,
                    words: own.words.into_iter().map(|(_, word)| word).collect(),
                    stdin: own.stdin.flatten(),
                });
            }
            "heredoc_redirect" => reader.heredoc_is_complete(*node)?,
            "word" | "string" | "raw_string" | "ansi_c_string" | "concatenation"
                if node.parent().is_none_or(|parent| parent.kind() != "concatenation") =>
            {
                script.words.push(reader.word(*node));
            }
            _ => {}
        }
    }
    Some(script)
}

/// The command that receives the redirections written after a statement.
fn target(mut node: Node<'_>) -> Option<Node<'_>> {
    loop {
        node = match node.kind() {
            "command" => return Some(node),
            "pipeline" | "list" | "negated_command" => {
                node.named_child(node.named_child_count().checked_sub(1)? as u32)?
            }
            "redirected_statement" => node.child_by_field_name("body")?,
            _ => return None,
        };
    }
}

struct Reader<'a> {
    text: &'a str,
    home: Option<&'a str>,
}

impl Reader<'_> {
    fn source(&self, node: Node<'_>) -> &str {
        &self.text[node.byte_range()]
    }

    fn redirect(&self, node: Node<'_>, into: &mut Attached) -> Option<()> {
        match node.kind() {
            "file_redirect" => {
                let parts = children(node);
                let closes = parts.iter().any(|(_, part)| matches!(part.kind(), "<&-" | ">&-"));
                let reads = parts.iter().any(|(_, part)| matches!(part.kind(), "<" | "<&"))
                    && parts.iter().all(|(_, part)| part.kind() != "file_descriptor" || self.source(*part) == "0");
                if reads {
                    into.input(None);
                }
                let destinations = parts.iter().filter(|(field, _)| *field == Some("destination"));
                for (_, destination) in destinations.skip(usize::from(!closes)) {
                    into.words.push((destination.start_byte(), self.word(*destination)));
                }
            }
            "herestring_redirect" => {
                let value = node.named_child(node.named_child_count().checked_sub(1)? as u32)?;
                let word = self.word(value);
                into.input((!word.dynamic && !word.glob).then_some(word.text));
            }
            "heredoc_redirect" => {
                let mut body = None;
                let mut quoted = false;
                for (field, child) in children(node) {
                    match child.kind() {
                        "heredoc_start" => quoted = self.source(child).contains(['\'', '"', '\\']),
                        "heredoc_body" => body = Some(child),
                        "file_redirect" | "herestring_redirect" => self.redirect(child, into)?,
                        _ if field == Some("argument") => {
                            into.words.push((child.start_byte(), self.word(child)));
                        }
                        _ => {}
                    }
                }
                into.input(body.map(|body| self.source(body))
                    .filter(|body| quoted || !body.contains(['$', '`', '\\']))
                    .map(str::to_owned));
            }
            "comment" => {}
            _ => return None,
        }
        Some(())
    }

    /// A substitution in an unquoted here-document runs, so the grammar must
    /// have seen every one of them.
    fn heredoc_is_complete(&self, node: Node<'_>) -> Option<()> {
        let parts = children(node);
        let quoted = parts.iter()
            .any(|(_, part)| part.kind() == "heredoc_start" && self.source(*part).contains(['\'', '"', '\\']));
        if !parts.iter().any(|(_, part)| part.kind() == "heredoc_end") {
            return None;
        }
        let Some((_, body)) = parts.iter().find(|(_, part)| part.kind() == "heredoc_body") else {
            return Some(());
        };
        if quoted {
            return Some(());
        }
        let seen: Vec<_> = nodes(*body).into_iter()
            .filter(|node| matches!(node.kind(), "command_substitution" | "arithmetic_expansion" | "expansion"))
            .map(|node| node.byte_range())
            .collect();
        let source = self.source(*body);
        ["$(", "`"].iter().all(|opening| source.match_indices(opening).all(|(index, _)| {
            let escaped = source[..index].chars().rev().take_while(|c| *c == '\\').count() % 2 == 1;
            let index = body.start_byte() + index;
            escaped || seen.iter().any(|range| range.start <= index && index <= range.end)
        })).then_some(())
    }

    fn assignment(&self, node: Node<'_>) -> Word {
        let name = node.child_by_field_name("name").map(|name| self.source(name)).unwrap_or_default();
        let mut value = node.child_by_field_name("value").map(|value| self.word(value)).unwrap_or_default();
        value.text = format!("{name}={}", value.text);
        value
    }

    fn word(&self, node: Node<'_>) -> Word {
        let mut word = Word::default();
        let mut bare = String::new();
        self.append(node, &mut word, &mut bare);
        // `a{b,c}` and `{1..3}` become several words.
        if bare.find('{').is_some_and(|open| {
            let rest = &bare[open..];
            rest.contains('}') && (rest.contains(',') || rest.contains(".."))
        }) {
            word.dynamic = true;
        }
        word
    }

    /// Add one node's value. `bare` collects the unquoted text.
    fn append(&self, node: Node<'_>, word: &mut Word, bare: &mut String) {
        let source = self.source(node);
        match node.kind() {
            "concatenation" => {
                let mut end = node.start_byte();
                for (_, child) in children(node) {
                    unquoted(&self.text[end..child.start_byte()], word, bare);
                    self.append(child, word, bare);
                    end = child.end_byte();
                }
                unquoted(&self.text[end..node.end_byte()], word, bare);
            }
            "word" | "number" if node.named_child_count() == 0 => unquoted(source, word, bare),
            "{" | "}" | "[" | "]" | "$" | "==" | "=~" | "test_operator" => unquoted(source, word, bare),
            "raw_string" if source.len() >= 2 => word.text.push_str(&source[1..source.len() - 1]),
            "ansi_c_string" if source.len() >= 3 && !source.contains('\\') => {
                word.text.push_str(&source[2..source.len() - 1]);
            }
            "translated_string" => match node.named_child(0) {
                Some(inner) => self.append(inner, word, bare),
                None => unknown(word),
            },
            "string" if source.len() >= 2 => {
                let mut end = node.start_byte() + 1;
                for (_, child) in children(node) {
                    if matches!(child.kind(), "\"" | "string_content") {
                        continue;
                    }
                    // A node can start at the blanks before its `$`.
                    let raw = self.source(child);
                    let start = child.start_byte() + raw.len() - raw.trim_start().len();
                    quoted(&self.text[end..start], word);
                    self.append(child, word, bare);
                    end = child.end_byte();
                }
                quoted(&self.text[end..node.end_byte() - 1], word);
            }
            "simple_expansion" | "expansion" => match self.home {
                Some(home) if matches!(source.trim(), "$HOME" | "${HOME}") => word.text.push_str(home),
                _ => unknown(word),
            },
            _ => unknown(word),
        }
    }
}

fn unknown(word: &mut Word) {
    word.text.push(UNKNOWN);
    word.dynamic = true;
}

fn unquoted(source: &str, word: &mut Word, bare: &mut String) {
    let mut chars = source.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some('\n') => {}
                Some(next) => word.text.push(next),
                None => word.text.push('\\'),
            },
            // An expansion the grammar left inside a plain word.
            '$' if chars.as_str().starts_with(['{', '(']) => unknown(word),
            _ => {
                word.glob |= matches!(c, '*' | '?') || c == '[' && chars.as_str().contains(']');
                word.text.push(c);
                bare.push(c);
            }
        }
    }
}

fn quoted(source: &str, word: &mut Word) {
    let mut chars = source.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.peek() {
                Some('"' | '\\' | '$' | '`') => word.text.extend(chars.next()),
                Some('\n') => {
                    chars.next();
                }
                _ => word.text.push(c),
            }
        } else {
            word.text.push(c);
        }
    }
}
