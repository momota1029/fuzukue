use std::{
    collections::HashMap,
    ops::Range,
    sync::{Arc, LazyLock, Mutex, Once},
};

use gpui_kit::component::highlighter::{GrammarConfig, LanguageRegistry};
use tree_sitter::{Language, Node, Parser, Query, QueryCursor, StreamingIterator};

pub(super) struct OutlineEntry {
    pub name: String,
    pub bytes: Range<usize>,
    pub depth: usize,
    pub editable: bool,
}

static QUERIES: LazyLock<Mutex<HashMap<&'static str, Option<Arc<Query>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub(super) fn register_grammars() {
    static INIT: Once = Once::new();
    INIT.call_once(register);
}

fn register() {
    let registry = LanguageRegistry::singleton();
    if let Some(mut markdown) = registry.language("markdown") {
        // The stock themes have no color for this capture. Keep the grammar,
        // inline injections and YAML metadata, and use the theme's attribute color.
        markdown.highlights = markdown.highlights
            .replace("@punctuation.list_marker", "@attribute")
            .into();
        registry.register("markdown", &markdown);
    }
    for (name, grammar, highlights) in [
        (
            "r",
            tree_sitter_r::LANGUAGE.into(),
            tree_sitter_r::HIGHLIGHTS_QUERY,
        ),
        ("julia", tree_sitter_julia::LANGUAGE.into(), ""),
        (
            "lean",
            arborium_lean::language().into(),
            arborium_lean::HIGHLIGHTS_QUERY,
        ),
        ("tex", tree_sitter_latex::LANGUAGE.into(), ""),
    ] {
        let fallback;
        let highlights = if highlights.is_empty() {
            fallback = basic_highlights(name, &grammar);
            &fallback
        } else {
            highlights
        };
        registry.register(
            name,
            &GrammarConfig::new(name, grammar, vec![], highlights, "", ""),
        );
    }
    // These grammars ship without a highlighting query in gpui-kit.
    for name in ["swift", "cmake"] {
        if let Some(mut config) = registry.language(name) {
            if let Some(grammar) = &config.language {
                config.highlights = basic_highlights(name, grammar).into();
                registry.register(name, &config);
            }
        }
    }
}

fn basic_highlights(name: &str, grammar: &Language) -> String {
    let mut query = String::new();
    for (kind, capture) in [
        ("line_comment", "comment"),
        ("block_comment", "comment"),
        ("comment", "comment"),
        ("string_literal", "string"),
        ("line_string_literal", "string"),
        ("multi_line_string_literal", "string"),
        ("string", "string"),
        ("character_literal", "string"),
        ("integer_literal", "number"),
        ("float_literal", "number"),
        ("real_literal", "number"),
        ("type_identifier", "type"),
        ("command_name", "function"),
        ("variable_ref", "variable"),
        ("command", "function"),
        ("generic_command", "function"),
        ("label_definition", "label"),
    ] {
        if named_node(grammar, kind) {
            query.push_str(&format!("({kind}) @{capture}\n"));
        }
    }
    let keywords = match name {
        "swift" => "func class struct enum protocol extension var let import if else return for in",
        "julia" => "function struct mutable abstract module macro let begin end if else for while return using import",
        "cmake" => "if else elseif endif function endfunction macro endmacro foreach endforeach",
        "tex" => r"\part \chapter \section \subsection \subsubsection \paragraph \begin \end",
        _ => "",
    };
    for word in keywords.split_whitespace() {
        if grammar.id_for_node_kind(word, false) != 0 {
            query.push_str(&format!("{word:?} @keyword\n"));
        }
    }
    query
}

fn named_node(grammar: &Language, name: &str) -> bool {
    grammar.id_for_node_kind(name, true) != 0
}

fn node_kinds(language: &str) -> &'static str {
    match language {
        "rust" => "function_item struct_item enum_item trait_item type_item union_item impl_item mod_item",
        "python" => "function_definition class_definition",
        "c" => "function_definition struct_specifier enum_specifier union_specifier type_definition",
        "cpp" => "function_definition struct_specifier enum_specifier union_specifier type_definition class_specifier namespace_definition alias_declaration",
        "go" => "function_declaration method_declaration type_declaration",
        "java" => "method_declaration constructor_declaration class_declaration interface_declaration enum_declaration record_declaration annotation_type_declaration",
        "javascript" => "function_declaration generator_function_declaration method_definition class_declaration variable_declarator",
        "typescript" | "tsx" => "function_declaration generator_function_declaration method_definition class_declaration variable_declarator interface_declaration type_alias_declaration enum_declaration internal_module",
        "bash" => "function_definition",
        "make" => "rule",
        "cmake" => "function_def macro_def",
        "lua" => "function_declaration",
        "zig" => "function_declaration variable_declaration",
        "kotlin" => "function_declaration class_declaration object_declaration type_alias",
        "swift" => "function_declaration class_declaration protocol_declaration typealias_declaration",
        "scala" => "function_definition class_definition trait_definition object_definition enum_definition type_definition",
        "ruby" => "method singleton_method class module",
        "r" => "binary_operator",
        "julia" => "function_definition short_function_definition macro_definition struct_definition abstract_definition primitive_definition module_definition",
        "lean" => "declaration section namespace",
        "markdown" => "atx_heading setext_heading",
        "tex" => "part chapter section subsection subsubsection paragraph subparagraph",
        "toml" => "table table_array_element",
        "yaml" => "block_mapping_pair",
        "json" => "pair",
        "sql" => "create_function create_procedure create_table create_view create_type",
        "html" => "element",
        "css" => "rule_set media_statement keyframes_statement",
        _ => "",
    }
}

fn build_query(language: &str, grammar: &Language) -> Option<Query> {
    let nodes = node_kinds(language)
        .split_whitespace()
        .filter(|kind| named_node(grammar, kind))
        .map(|kind| format!("({kind}) @item"))
        .collect::<Vec<_>>();
    if nodes.is_empty() {
        return None;
    }
    Query::new(grammar, &nodes.join("\n")).ok()
}

pub(super) fn extract(language: &'static str, text: &str) -> Vec<OutlineEntry> {
    register_grammars();
    let Some(grammar) = LanguageRegistry::singleton()
        .language(language)
        .and_then(|config| config.language)
    else {
        return Vec::new();
    };
    let query = QUERIES
        .lock()
        .unwrap()
        .entry(language)
        .or_insert_with(|| build_query(language, &grammar).map(Arc::new))
        .clone();
    let Some(query) = query else {
        return Vec::new();
    };
    let mut parser = Parser::new();
    if parser.set_language(&grammar).is_err() {
        return Vec::new();
    }
    let Some(tree) = parser.parse(text, None) else {
        return Vec::new();
    };
    let mut cursor = QueryCursor::new();
    let source = text.as_bytes();
    let mut captures = cursor.captures(&query, tree.root_node(), source);
    let mut entries = Vec::<OutlineEntry>::new();
    let mut parents = Vec::<usize>::new();
    while let Some((matched, index)) = captures.next() {
        let node = matched.captures[*index].node;
        let Some((name, bytes, editable)) = entry(language, node, text) else {
            continue;
        };
        if entries.last().is_some_and(|entry| entry.bytes == bytes) {
            continue;
        }
        while parents.last().is_some_and(|&end| end <= bytes.start) {
            parents.pop();
        }
        // Markdown sections do not always nest in the tree (setext headings), so the level decides.
        let depth = if language == "markdown" { markdown_level(node).saturating_sub(1) } else { parents.len() };
        parents.push(bytes.end);
        entries.push(OutlineEntry {
            name,
            bytes,
            depth,
            editable,
        });
    }
    entries
}

fn markdown_level(node: Node<'_>) -> usize {
    let mut cursor = node.walk();
    let level = node.children(&mut cursor)
        .find_map(|child| {
            let kind = child.kind();
            kind.strip_prefix("atx_h")
                .and_then(|rest| rest.strip_suffix("_marker"))
                .and_then(|level| level.parse().ok())
                .or_else(|| match kind {
                    "setext_h1_underline" => Some(1),
                    "setext_h2_underline" => Some(2),
                    _ => None,
                })
        })
        .unwrap_or(1);
    level
}

fn entry(language: &str, node: Node<'_>, text: &str) -> Option<(String, Range<usize>, bool)> {
    let mut editable = !matches!(
        language,
        "markdown" | "tex" | "make" | "cmake" | "toml" | "yaml" | "json" | "html" | "css"
    );
    let mut range = node.byte_range();
    let mut cursor = node.walk();
    let first = || node.named_child(0);
    let field = |name| node.child_by_field_name(name);
    let label = match language {
        "markdown" => {
            if let Some(parent) = node.parent().filter(|parent| parent.kind() == "section") {
                range.end = parent.end_byte();
            }
            field("heading_content").or_else(|| {
                node.named_children(&mut cursor)
                    .find(|child| matches!(child.kind(), "inline" | "paragraph"))
            })?
        }
        "tex" => field("text").or_else(|| {
            node.named_children(&mut cursor)
                .find(|child| matches!(child.kind(), "curly_group" | "curly_group_text"))
        })?,
        "python" => {
            if let Some(parent) = node
                .parent()
                .filter(|parent| parent.kind() == "decorated_definition")
            {
                range = parent.byte_range();
            }
            field("name")?
        }
        "lean" if node.kind() == "declaration" => node
            .named_children(&mut cursor)
            .find_map(|child| child.child_by_field_name("name"))?,
        "lean" => {
            editable = false;
            field("name")?
        }
        "javascript" | "typescript" | "tsx" if node.kind() == "variable_declarator" => {
            if !matches!(
                field("value")?.kind(),
                "arrow_function" | "function_expression" | "generator_function"
            ) {
                return None;
            }
            let declaration = node.parent()?;
            let mut siblings = declaration.walk();
            editable = declaration
                .named_children(&mut siblings)
                .filter(|child| child.kind() == "variable_declarator")
                .count()
                == 1;
            if editable {
                range = declaration.byte_range();
            }
            field("name")?
        }
        "c" | "cpp"
            if matches!(
                node.kind(),
                "struct_specifier" | "enum_specifier" | "class_specifier" | "union_specifier"
            ) =>
        {
            field("body")?;
            field("name")?
        }
        "julia"
            if matches!(
                node.kind(),
                "function_definition" | "short_function_definition" | "macro_definition"
            ) =>
        {
            let signature = first()?;
            match signature.kind() {
                "signature" => signature
                    .named_child(0)?
                    .named_child(0)
                    .or_else(|| signature.named_child(0))?,
                _ => signature,
            }
        }
        "r" => {
            let lhs = field("lhs")?;
            let rhs = field("rhs")?;
            let operator = field("operator").and_then(|op| text.get(op.byte_range()))?;
            match operator {
                "<-" | "<<-" | "=" if rhs.kind() == "function_definition" => lhs,
                "->" | "->>" if lhs.kind() == "function_definition" => rhs,
                _ => return None,
            }
        }
        "zig" if node.kind() == "variable_declaration" => {
            if !node.named_children(&mut cursor).any(|child| {
                matches!(
                    child.kind(),
                    "struct_declaration"
                        | "enum_declaration"
                        | "union_declaration"
                        | "opaque_declaration"
                )
            }) {
                return None;
            }
            field("name").or_else(first)?
        }
        "rust" if node.kind() == "impl_item" => {
            editable = false;
            field("type")?
        }
        "go" if node.kind() == "type_declaration" => first()?.child_by_field_name("name")?,
        "html" => {
            let tag = first()?.named_child(0)?;
            if !matches!(
                text.get(tag.byte_range())?,
                "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "section" | "article" | "main"
            ) {
                return None;
            }
            tag
        }
        _ => field("name")
            .or_else(|| field("key"))
            .or_else(|| field("target"))
            .or_else(|| field("declarator").and_then(declarator_name))
            .or_else(|| {
                node.named_children(&mut cursor).find(|child| {
                    matches!(
                        child.kind(),
                        "identifier" | "simple_identifier" | "type_identifier"
                    )
                })
            })
            .or_else(first)?,
    };
    let mut name = text.get(label.byte_range())?.lines().next()?.trim();
    if language == "tex" {
        name = name
            .strip_prefix('{')
            .and_then(|name| name.strip_suffix('}'))
            .unwrap_or(name)
            .trim();
    }
    if name.is_empty() {
        return None;
    }
    let name = name.chars().take(120).collect();
    Some((name, range, editable))
}

fn declarator_name(node: Node<'_>) -> Option<Node<'_>> {
    if matches!(
        node.kind(),
        "identifier" | "field_identifier" | "type_identifier" | "qualified_identifier"
    ) {
        Some(node)
    } else {
        node.child_by_field_name("declarator")
            .and_then(declarator_name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_rust_types_and_nested_functions() {
        let text = "struct Example { n: usize }\nimpl Example {\n    fn measure(&self) -> usize { self.n }\n}\n";
        let entries = extract("rust", text);
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            ["Example", "Example", "measure"]
        );
        assert_eq!(entries[2].depth, 1);
        assert!(entries[2].editable);
        assert!(text[entries[2].bytes.clone()].starts_with("fn measure"));
    }

    #[test]
    fn extracts_python_class_and_decorated_method() {
        let text = "class Model:\n    @staticmethod\n    def fit():\n        return 1\n";
        let entries = extract("python", text);
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            ["Model", "fit"]
        );
        assert_eq!(entries[1].depth, 1);
        assert!(text[entries[1].bytes.clone()].starts_with("@staticmethod"));
    }

    #[test]
    fn extracts_markdown_headings() {
        let entries = extract("markdown", "実験\n====\n\n本文\n\n## 結果\n\n本文\n");
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            ["実験", "結果"]
        );
        assert_eq!(entries[1].depth, 1);
        assert!(entries.iter().all(|entry| !entry.editable));
    }

    #[test]
    fn external_grammars_register_for_highlighting_and_outline() {
        register_grammars();
        for language in ["r", "julia", "lean", "tex", "swift", "cmake"] {
            let config = LanguageRegistry::singleton().language(language).unwrap();
            let grammar = config.language.unwrap();
            assert!(Parser::new().set_language(&grammar).is_ok(), "{language}");
            assert!(!config.highlights.is_empty(), "{language}");
            assert!(
                Query::new(&grammar, &config.highlights).is_ok(),
                "{language}"
            );
        }
        let tex = extract(
            "tex",
            r"\section{実験}
\subsection{結果}",
        );
        assert_eq!(
            tex.iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            ["実験", "結果"]
        );
    }
}
