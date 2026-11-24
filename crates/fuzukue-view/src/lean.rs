//! Source-level Lean documentation and declaration boundaries.
use fuzukue_proto::LspPosition;
use tree_sitter::Node;

#[derive(Debug, Default, Clone)]
pub struct Page {
    pub items: Vec<Item>,
    pub incomplete: bool,
}

#[derive(Debug, Clone)]
pub enum Item {
    Prose(String),
    Declaration(Box<Declaration>),
}

#[derive(Debug, Clone)]
pub struct Declaration {
    pub name: String,
    pub kind: String,
    pub public: bool,
    pub position: LspPosition,
    pub doc: String,
    pub card: Option<String>,
    pub statement: String,
    pub binders: Vec<Binder>,
    pub conclusion: String,
    pub context: Vec<String>,
    pub proof: String,
    pub steps: Vec<Step>,
    signature: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Binder {
    pub text: String,
    pub implicit: bool,
}

#[derive(Debug, Clone)]
pub struct Step {
    pub heading: String,
    pub code: String,
    pub before: LspPosition,
    pub after: LspPosition,
}

impl Declaration {
    pub fn same_statement(&self, other: &Self) -> bool {
        self.kind == other.kind
            && self.public == other.public
            && self.signature == other.signature
            && self.context == other.context
    }

    pub fn claim(&self) -> String {
        self.context
            .iter()
            .chain(std::iter::once(&self.statement))
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn needs_doc(&self) -> bool {
        self.public
            && matches!(
                self.kind.as_str(),
                "theorem" | "def" | "abbrev" | "structure"
            )
    }
}

impl Page {
    pub fn declarations(&self) -> impl Iterator<Item = &Declaration> {
        self.items.iter().filter_map(|item| match item {
            Item::Declaration(decl) => Some(decl.as_ref()),
            Item::Prose(_) => None,
        })
    }
}

pub fn parse(source: &str) -> Page {
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&arborium_lean::language().into())
        .expect("bundled Lean grammar");
    let Some(tree) = parser.parse(source, None) else {
        return Page {
            incomplete: true,
            ..Page::default()
        };
    };
    // The grammar recognizes section blocks. A mutual block has the same
    // command boundaries, so use a section surrogate for the parser.
    let mut offset = 0;
    let mut replacements = Vec::new();
    for line in source.split_inclusive('\n') {
        let start = offset + line.len() - line.trim_start().len();
        if line.trim() == "mutual"
            && tree
                .root_node()
                .descendant_for_byte_range(start, start + 6)
                .is_some_and(|n| {
                    !std::iter::successors(Some(n), |n| n.parent()).any(|n| {
                        is_comment(n) || matches!(n.kind(), "string" | "interpolated_string")
                    })
                })
        {
            replacements.push(start);
        }
        offset += line.len();
    }
    let mut normalized = String::new();
    let (source, tree) = if replacements.is_empty() {
        (source, tree)
    } else {
        normalized.push_str(source);
        for start in replacements.into_iter().rev() {
            normalized.replace_range(start..start + 6, "section");
        }
        let tree = parser
            .parse(&normalized, None)
            .expect("bundled Lean grammar");
        (normalized.as_str(), tree)
    };
    let root = tree.root_node();
    let mut page = Page {
        incomplete: root.has_error(),
        ..Page::default()
    };
    let mut context = Vec::new();
    visit(root, source, "", &mut context, &mut page);
    page
}

fn children(node: Node<'_>) -> Vec<Node<'_>> {
    node.children(&mut node.walk()).collect()
}

fn text<'a>(node: Node<'_>, source: &'a str) -> &'a str {
    &source[node.byte_range()]
}

pub fn position(source: &str, offset: usize) -> LspPosition {
    let prefix = &source[..offset];
    let line = prefix.bytes().filter(|&b| b == b'\n').count() as u32;
    let character = prefix
        .rsplit('\n')
        .next()
        .unwrap_or("")
        .encode_utf16()
        .count() as u32;
    LspPosition { line, character }
}

fn visit(
    node: Node<'_>,
    source: &str,
    namespace: &str,
    context: &mut Vec<String>,
    page: &mut Page,
) {
    let mut doc = String::new();
    let mut card = None;
    for child in children(node) {
        let raw = text(child, source);
        match child.kind() {
            "comment" | "block_comment" => {
                if let Some(body) = raw.strip_prefix("/-!").and_then(|s| s.strip_suffix("-/")) {
                    page.items.push(Item::Prose(body.trim().into()));
                    doc.clear();
                    card = None;
                } else if let Some(body) =
                    raw.strip_prefix("/--").and_then(|s| s.strip_suffix("-/"))
                {
                    doc = body.trim().into();
                } else if let Some(id) = card_id(raw) {
                    card = Some(id.into());
                }
                continue;
            }
            "namespace" => {
                let name = child
                    .child_by_field_name("name")
                    .map(|n| text(n, source))
                    .unwrap_or("");
                visit(
                    child,
                    source,
                    &qualify(namespace, name),
                    &mut context.clone(),
                    page,
                );
            }
            "section" | "mutual" => visit(child, source, namespace, &mut context.clone(), page),
            "ERROR" => visit(child, source, namespace, context, page),
            "variable" | "universe" | "include" | "omit" => context.push(raw.trim().into()),
            "declaration" => {
                if let Some(decl) = declaration(
                    child,
                    source,
                    namespace,
                    context,
                    std::mem::take(&mut doc),
                    card.take(),
                ) {
                    page.items.push(Item::Declaration(Box::new(decl)));
                }
            }
            // Some wrapper and scope tokens precede the first declaration.
            _ => {}
        }
        doc.clear();
        card = None;
    }
}

fn qualify(namespace: &str, name: &str) -> String {
    if let Some(name) = name.strip_prefix("_root_.") {
        name.into()
    } else if namespace.is_empty() {
        name.into()
    } else {
        format!("{namespace}.{name}")
    }
}

fn card_id(comment: &str) -> Option<&str> {
    let id = comment
        .strip_prefix("--")?
        .trim()
        .strip_prefix("card:")?
        .trim();
    (!id.is_empty() && !id.chars().any(char::is_whitespace)).then_some(id)
}

fn declaration(
    wrapper: Node<'_>,
    source: &str,
    namespace: &str,
    context: &[String],
    doc: String,
    card: Option<String>,
) -> Option<Declaration> {
    let parts = children(wrapper);
    let decl = parts.iter().copied().find(|n| {
        matches!(
            n.kind(),
            "theorem"
                | "def"
                | "abbrev"
                | "structure"
                | "inductive"
                | "instance"
                | "class_inductive"
                | "axiom"
                | "constant"
        )
    })?;
    let name = decl.child_by_field_name("name")?;
    let public = !parts.iter().any(|n| text(*n, source).trim() == "private");
    let kind = decl.kind().to_owned();
    let body = decl
        .child_by_field_name("body")
        .filter(|_| !matches!(kind.as_str(), "structure" | "inductive"));
    let end = body.map_or(decl.end_byte(), |n| n.start_byte());
    let raw = &source[decl.start_byte()..end];
    let decl_parts = children(decl);
    let binder_list = decl_parts.iter().find(|n| n.kind() == "binders");
    let binders = binder_list
        .into_iter()
        .flat_map(|n| children(*n))
        .filter(|n| n.is_named() && !is_comment(*n))
        .map(|n| Binder {
            text: text(n, source).trim().into(),
            implicit: matches!(n.kind(), "implicit_binder" | "instance_binder"),
        })
        .collect();
    let conclusion_start = binder_list.map_or(name.end_byte(), |n| n.end_byte());
    let conclusion = if conclusion_start <= end {
        source[conclusion_start..end]
            .trim()
            .trim_start_matches(':')
            .trim()
            .into()
    } else {
        String::new()
    };
    let mut signature = Vec::new();
    tokens(decl, source, end, &mut signature);
    let mut comments = Vec::new();
    collect_comments(wrapper, &mut comments);
    let card = card.or_else(|| {
        comments
            .iter()
            .find_map(|n| card_id(text(*n, source)).map(str::to_owned))
    });
    let doc = if doc.is_empty() {
        comments
            .iter()
            .filter(|n| n.end_byte() <= decl.start_byte())
            .find_map(|n| {
                Some(
                    text(*n, source)
                        .strip_prefix("/--")?
                        .strip_suffix("-/")?
                        .trim()
                        .into(),
                )
            })
            .unwrap_or_default()
    } else {
        doc
    };
    let proof = body
        .map_or("", |n| &source[n.end_byte()..decl.end_byte()])
        .trim()
        .to_owned();
    let steps = body.map_or_else(Vec::new, |n| {
        proof_steps(source, decl, n.end_byte(), &comments)
    });
    Some(Declaration {
        name: qualify(namespace, text(name, source)),
        kind,
        public,
        position: position(source, name.start_byte()),
        doc,
        card,
        statement: raw.trim().into(),
        binders,
        conclusion,
        context: context.to_vec(),
        proof,
        steps,
        signature,
    })
}

fn is_comment(node: Node<'_>) -> bool {
    matches!(node.kind(), "comment" | "block_comment")
}

fn tokens(node: Node<'_>, source: &str, end: usize, out: &mut Vec<String>) {
    if node.start_byte() >= end || is_comment(node) || node.is_missing() {
        return;
    }
    if node.child_count() == 0 {
        out.push(text(node, source).into());
    } else {
        for child in children(node) {
            tokens(child, source, end, out);
        }
    }
}

fn collect_comments<'a>(node: Node<'a>, out: &mut Vec<Node<'a>>) {
    if is_comment(node) {
        out.push(node);
    } else if node.child_count() > 0 {
        for child in children(node) {
            collect_comments(child, out);
        }
    }
}

fn proof_steps(source: &str, decl: Node<'_>, start: usize, comments: &[Node<'_>]) -> Vec<Step> {
    let headings: Vec<_> = comments
        .iter()
        .filter(|n| {
            let raw = text(**n, source);
            n.start_byte() >= start
                && raw.starts_with("--")
                && card_id(raw).is_none()
                && !raw[2..].trim().is_empty()
                && source[..n.start_byte()]
                    .rsplit('\n')
                    .next()
                    .is_some_and(|s| s.trim().is_empty())
        })
        .collect();
    headings
        .iter()
        .enumerate()
        .map(|(i, n)| {
            let end = headings
                .get(i + 1)
                .map_or(decl.end_byte(), |next| next.start_byte());
            let code = &source[n.end_byte()..end];
            let trimmed = code.trim();
            Step {
                heading: text(**n, source)[2..].trim().into(),
                code: trimmed.into(),
                before: position(source, end - code.trim_start().len()),
                after: position(source, n.end_byte() + code.trim_end().len()),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_literate_declarations() {
        let source = r#"import Init
/-! # A module with $x$. -/
namespace Example
section
variable (n : Nat)
/-- A documented result. -/
-- card: example-card
@[simp] theorem result {α : Type} [Inhabited α] (x : α) : x = x := by
  -- Reflexivity closes the goal.
  rfl

/-- A structure. -/
structure Box where
  value : Nat

mutual
  /-- First. -/
  def first : Nat := 1
  /-- Second. -/
  def second : Nat := first
end

private theorem hidden : True := trivial
end
end Example
"#;
        let page = parse(source);
        let decls: Vec<_> = page.declarations().collect();
        assert!(
            matches!(&page.items[0], Item::Prose(p) if p.contains("$x$")),
            "{page:#?}"
        );
        assert_eq!(
            decls.iter().map(|d| d.name.as_str()).collect::<Vec<_>>(),
            [
                "Example.result",
                "Example.Box",
                "Example.first",
                "Example.second",
                "Example.hidden"
            ],
            "{page:#?}"
        );
        let result = decls[0];
        assert_eq!(result.doc, "A documented result.");
        assert_eq!(result.card.as_deref(), Some("example-card"));
        assert_eq!(result.binders.len(), 3);
        assert!(result.binders[0].implicit && result.binders[1].implicit);
        assert_eq!(result.conclusion, "x = x");
        assert_eq!(result.steps[0].code, "rfl");
        assert!(decls[1].conclusion.contains("value : Nat"));
        assert!(!decls[4].needs_doc());
    }
}
