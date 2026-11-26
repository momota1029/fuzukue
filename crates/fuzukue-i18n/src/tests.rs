use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

use fluent_syntax::{ast, parser};
use syn::{visit::Visit, LitStr, Macro};

use super::*;

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn sources(path: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for entry in fs::read_dir(path).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            files.extend(sources(&path));
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            files.push(path);
        }
    }
    files
}

fn message_inputs<'a>(
    pattern: &ast::Pattern<&'a str>,
    vars: &mut BTreeSet<String>,
    references: &mut BTreeSet<&'a str>,
) {
    fn inline<'a>(
        expression: &ast::InlineExpression<&'a str>,
        vars: &mut BTreeSet<String>,
        references: &mut BTreeSet<&'a str>,
    ) {
        match expression {
            ast::InlineExpression::VariableReference { id } => {
                vars.insert(id.name.into());
            }
            ast::InlineExpression::MessageReference {
                id,
                attribute: None,
            } => {
                references.insert(id.name);
            }
            _ => {}
        }
    }
    for element in &pattern.elements {
        if let ast::PatternElement::Placeable { expression } = element {
            match expression {
                ast::Expression::Inline(expression) => inline(expression, vars, references),
                ast::Expression::Select { selector, variants } => {
                    inline(selector, vars, references);
                    for variant in variants {
                        message_inputs(&variant.value, vars, references);
                    }
                }
            }
        }
    }
}

#[derive(Default)]
struct Literals {
    strings: BTreeSet<String>,
    translation_keys: BTreeSet<String>,
    skip_tests: bool,
}

impl Literals {
    fn tokens(&mut self, tokens: proc_macro2::TokenStream) {
        for token in tokens {
            match token {
                proc_macro2::TokenTree::Group(group) => self.tokens(group.stream()),
                proc_macro2::TokenTree::Literal(literal) => {
                    if let Ok(text) = syn::parse_str::<LitStr>(&literal.to_string()) {
                        self.visit_lit_str(&text);
                    }
                }
                _ => {}
            }
        }
    }
}

impl<'ast> Visit<'ast> for Literals {
    fn visit_lit_str(&mut self, literal: &'ast LitStr) {
        self.strings.insert(literal.value());
    }

    fn visit_macro(&mut self, mac: &'ast Macro) {
        if mac.path.is_ident("tr") {
            if let Some(token) = mac.tokens.clone().into_iter().next() {
                if let Ok(key) = syn::parse_str::<LitStr>(&token.to_string()) {
                    self.translation_keys.insert(key.value());
                }
            }
        }
        self.tokens(mac.tokens.clone());
    }

    fn visit_attribute(&mut self, _: &'ast syn::Attribute) {}

    fn visit_item_mod(&mut self, module: &'ast syn::ItemMod) {
        if !self.skip_tests
            || !module.attrs.iter().any(|attr| {
                attr.path().is_ident("cfg")
                    && attr
                        .parse_args::<syn::Ident>()
                        .is_ok_and(|ident| ident == "test")
            })
        {
            syn::visit::visit_item_mod(self, module);
        }
    }
}

#[test]
fn catalogs_parse_and_match_code_and_each_other() {
    let mut reference = None;
    let mut referenced_messages = BTreeSet::new();
    for &(tag, files) in RESOURCES {
        let mut patterns = BTreeMap::new();
        for &file in files {
            let resource =
                parser::parse(file).unwrap_or_else(|(_, errors)| panic!("{tag}: {errors:?}"));
            for entry in resource.body {
                if let ast::Entry::Message(message) = entry {
                    let key = message.id.name;
                    assert!(
                        patterns
                            .insert(key, message.value.expect("message needs a value"))
                            .is_none(),
                        "{tag}: duplicate {key}"
                    );
                }
            }
        }
        let mut messages = BTreeMap::new();
        for &key in patterns.keys() {
            let mut vars = BTreeSet::new();
            let mut visited = BTreeSet::new();
            let mut pending = vec![key];
            while let Some(name) = pending.pop() {
                if !visited.insert(name) {
                    continue;
                }
                let mut references = BTreeSet::new();
                let pattern = patterns
                    .get(name)
                    .unwrap_or_else(|| panic!("{tag}: {key} references missing message {name}"));
                message_inputs(pattern, &mut vars, &mut references);
                pending.extend(references.iter().copied());
                referenced_messages.extend(references);
            }
            messages.insert(key, vars);
        }
        if let Some(reference) = &reference {
            assert_eq!(&messages, reference, "keys and variables in {tag}");
        } else {
            reference = Some(messages);
        }
    }
    let messages = reference.unwrap();
    for locale in Locale::available() {
        for (key, vars) in &messages {
            let mut args = FluentArgs::new();
            for var in vars {
                args.set(var.as_str(), 1);
            }
            let bundle = &locale.catalog().bundle;
            let value = bundle.get_message(key).unwrap().value().unwrap();
            let mut errors = Vec::new();
            bundle.format_pattern(value, Some(&args), &mut errors);
            assert!(errors.is_empty(), "{} {key}: {errors:?}", locale.tag());
        }
    }
    let mut usage = Literals {
        skip_tests: true,
        ..Default::default()
    };
    for component in ["fuzukue", "secretaire", "fuzukue-i18n", "fuzukue-proto", "fuzukue-view"] {
        for path in sources(&root().join("crates").join(component).join("src")) {
            usage.visit_file(&syn::parse_file(&fs::read_to_string(path).unwrap()).unwrap());
        }
    }
    // The selector builds language option keys from the same locale list as the bundles.
    for tag in std::iter::once("system").chain(Locale::available().map(Locale::tag)) {
        let key = format!("settings-language-{tag}");
        usage.translation_keys.insert(key.clone());
        usage.strings.insert(key);
    }
    let missing: Vec<_> = usage
        .translation_keys
        .iter()
        .filter(|key| !messages.contains_key(key.as_str()))
        .collect();
    let unused: Vec<_> = messages
        .keys()
        .filter(|key| !usage.strings.contains(**key) && !referenced_messages.contains(**key))
        .collect();
    assert!(
        missing.is_empty() && unused.is_empty(),
        "missing: {missing:?}; unused: {unused:?}"
    );
}

#[test]
fn goal_completion_pause_and_blocker_labels_are_localized() {
    let mut args = crate::FluentArgs::new();
    args.set("reason", "Check the real device");
    for (tag, tail, status, paused, blocker) in [
        ("ja", ["完了", "確認をお願いします（Check the real device）"], "完了（確認待ち）",
            "一時停止中", "確認が必要: Check the real device"),
        ("en", ["Done", "please check (Check the real device)"], "Done (verification pending)",
            "Paused", "Needs confirmation: Check the real device"),
    ] {
        let locale = Locale::supported(tag).unwrap();
        assert_eq!(locale.format("chat-goal-verification", &args).split(STATUS_SEPARATOR).collect::<Vec<_>>(), tail);
        assert_eq!(locale.text("thread-details-goal-verification"), status);
        assert_eq!(locale.text("chat-goal-paused"), paused);
        assert_eq!(locale.format("thread-details-step-blocked", &args), blocker);
    }
}

#[test]
fn chat_status_counts_are_localized() {
    let mut count = crate::FluentArgs::new();
    count.set("count", 3);
    for (tag, jobs, workers) in [("ja", "3 件", "作業役 3 件"), ("en", "3 jobs", "3 workers")] {
        let locale = Locale::supported(tag).unwrap();
        assert_eq!(locale.format("chat-status-count", &count), jobs);
        assert_eq!(locale.format("chat-status-workers", &count), workers);
    }
}

#[test]
fn model_setup_and_weekly_usage_are_localized() {
    let mut args = crate::FluentArgs::new();
    args.set("uses", "3");
    args.set("tokens", "120");
    for (tag, heading, later, usage) in [
        ("ja", "モデルと使用量", "あとで", ["過去7日間 3 回", "計測 120 トークン"]),
        ("en", "Models and usage", "Later", ["Past 7 days: 3 runs", "120 measured tokens"]),
    ] {
        let locale = Locale::supported(tag).unwrap();
        assert_eq!(locale.text("model-usage-heading"), heading);
        assert_eq!(locale.text("model-setup-later"), later);
        assert_eq!(locale.format("model-usage-weekly", &args).split(STATUS_SEPARATOR).collect::<Vec<_>>(), usage);
    }
}

#[test]
fn approval_dock_formats_counts_and_resolution_notices() {
    let ja = Locale::JAPANESE;
    let en = Locale::supported("en").unwrap();
    let mut args = FluentArgs::new();
    args.set("count", 2);
    for (key, japanese, english) in [
        ("approval-dock-pending", "2件の承認待ち", "2 pending"),
        ("approval-dock-pending-badge", "2 件の承認待ち", "2 pending approvals"),
        ("approval-dock-review-count", "2件の依頼", "2 requests"),
        ("approval-dock-conflicts", "2件はすでに解決済み", "2 already resolved"),
    ] {
        assert_eq!(ja.format(key, &args), japanese);
        assert_eq!(en.format(key, &args), english);
    }
    args.set("count", 1);
    assert_eq!(en.format("approval-dock-pending-badge", &args), "1 pending approval");
    assert_eq!(en.format("approval-dock-review-count", &args), "1 request");

    assert_eq!(ja.format("chat-auto-approved-count", &args), "自動許可 1 件");
    assert_eq!(en.format("chat-auto-approved-count", &args), "1 automatically allowed");
}

#[test]
fn desktop_status_catalogs_use_rendered_separators() {
    for locale in Locale::available() {
        for file in ["chat.ftl", "settings.ftl", "thread-details.ftl", "agents.ftl", "tiers.ftl"] {
            let path = root().join("locales").join(locale.tag()).join(file);
            let source = fs::read_to_string(&path).unwrap();
            assert!(!source.contains(" · "), "literal status separator in {}", path.display());
        }
    }
}

#[test]
fn ui_source_has_no_new_japanese_literals() {
    let client = root().join("crates/fuzukue/src");
    let mut unexpected = Vec::new();
    for path in sources(&client) {
        let relative = path.strip_prefix(&client).unwrap().to_str().unwrap();
        if matches!(
            relative,
            "math/selection_tests.rs"
                | "cjk_emphasis_tests.rs"
                | "shot.rs" // Japanese fixture text for headless scenarios.
                | "shot/file_browser_test.rs" // Japanese fixture names for the folder-tree layout test.
                // Japanese fixture text for headless selection/deck tests.
                | "shot/selection_region.rs"
                | "shot/deck_overview_test.rs"
        ) {
            continue;
        }
        let mut literals = Literals {
            skip_tests: true,
            ..Default::default()
        };
        literals.visit_file(&syn::parse_file(&fs::read_to_string(&path).unwrap()).unwrap());
        for text in literals.strings {
            let japanese = text
                .chars()
                .any(|ch| matches!(ch, '\u{3040}'..='\u{30ff}' | '\u{3400}'..='\u{9fff}'));
            let allowed = matches!(
                (relative, text.as_str()),
                ("appearance/font_picker.rs", "游ゴシック UI" | "メイリオ" | "游明朝" | "BIZ UD明朝" | "BIZ UDゴシック")
                | ("paper.rs", "主張" | "証明") // Source document headings.
                | ("thread_info.rs", "情報欄からの見直し依頼") // Planner input.
                | ("math/glyphs.rs", "KaTeX フォントを登録できない: {error:#}") // Diagnostic log.
            );
            if japanese && !allowed {
                unexpected.push((relative.to_owned(), text));
            }
        }
    }
    assert!(
        unexpected.is_empty(),
        "Japanese literals outside catalogs: {unexpected:?}"
    );
}

#[test]
fn supported_tags_and_plural_rules() {
    let en = Locale::supported("en_US").unwrap();
    assert_eq!(en.tag(), "en");
    assert_eq!(Locale::supported("ja-JP"), Some(Locale::JAPANESE));
    assert_eq!(Locale::supported("fr-FR"), None);
    let mut args = FluentArgs::new();
    args.set("calls", 1);
    assert_eq!(en.format("chat-tool-calls", &args), "1 tool call");
    args.set("calls", 2);
    assert_eq!(en.format("chat-tool-calls", &args), "2 tool calls");
    assert_eq!(
        Locale::JAPANESE.format("chat-tool-calls", &args),
        "ツール 2 件"
    );
}
#[test]
fn style_notes_use_localized_copy_and_keep_unknown_messages() {
    let ja = Locale::JAPANESE;
    let en = Locale::supported("en").unwrap();
    assert_eq!(
        ja.style_note_message("style/no-doubled-joshi", None, "ignored", None),
        "同じ助詞が近くで連続しています"
    );
    assert_eq!(
        en.style_note_message("style/textlint", Some("ja-technical-writing/max-ten"), "", Some(3)),
        "This sentence has many commas (aim for 3 or fewer)"
    );
    assert_eq!(
        ja.style_note_message("style/comma-density", None, "", Some(4)),
        "読点が多めです（目安 4 個）"
    );
    assert_eq!(
        en.style_note_message("style/textlint", Some("custom/check"), "Keep the custom message.", None),
        "Keep the custom message."
    );
    let mut args = FluentArgs::new();
    args.set("count", 1);
    assert_eq!(en.format("style-notes-summary", &args), "1 style note");
    args.set("count", 12);
    assert_eq!(en.format("style-notes-summary", &args), "12 style notes");
    assert_eq!(ja.format("style-notes-summary", &args), "文体の指摘 12 件");
}
