//! Embedded Fluent catalogs. The caller chooses the locale for each lookup.

use std::{collections::HashMap, sync::LazyLock};

pub use fluent_bundle::FluentArgs;
use fluent_bundle::{concurrent::FluentBundle, FluentResource, FluentValue};

/// Internal delimiter between status pieces that the desktop UI lays out separately.
pub const STATUS_SEPARATOR: &str = "\u{001f}";

include!(concat!(env!("OUT_DIR"), "/catalogs.rs"));

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Locale(&'static str);

impl Locale {
    pub const JAPANESE: Self = Self("ja");

    pub fn available() -> impl Iterator<Item = Self> {
        RESOURCES.iter().map(|(tag, _)| Self(tag))
    }

    /// Accept a regional tag, trying parent tags before falling back to Japanese.
    pub fn supported(tag: &str) -> Option<Self> {
        let mut tag = tag.replace('_', "-");
        loop {
            if let Some(locale) =
                Self::available().find(|locale| tag.eq_ignore_ascii_case(locale.0))
            {
                return Some(locale);
            }
            tag.truncate(tag.rfind('-')?);
        }
    }

    pub fn tag(self) -> &'static str {
        self.0
    }

    pub fn text(self, key: &str) -> &'static str {
        self.catalog()
            .plain
            .get(key)
            .or_else(|| Self::JAPANESE.catalog().plain.get(key))
            .map(String::as_str)
            .unwrap_or("�")
    }

    pub fn format(self, key: &str, args: &FluentArgs<'_>) -> String {
        for locale in [self, Self::JAPANESE] {
            let bundle = &locale.catalog().bundle;
            if let Some(value) = bundle.get_message(key).and_then(|message| message.value()) {
                let mut errors = Vec::new();
                let text = bundle.format_pattern(value, Some(args), &mut errors);
                if errors.is_empty() {
                    return text.into_owned();
                }
            }
        }
        key.to_owned()
    }

    pub fn style_fix_hint(self, code: &str, rule: Option<&str>) -> &'static str {
        self.text(match style_rule_name(code, rule) {
            "ai-phrase" => "style-hint-ai-phrase",
            "long-sentence" => "style-hint-long-sentence",
            "repeated-ending" => "style-hint-repeated-ending",
            "comma-density" => "style-hint-comma-density",
            "rhetorical-negation" => "style-hint-rhetorical-negation",
            "hedging" => "style-hint-hedging",
            "no-mix-dearu-desumasu" => "style-hint-no-mix-dearu-desumasu",
            "ja-no-mixed-period" => "style-hint-ja-no-mixed-period",
            "max-kanji-continuous-len" => "style-hint-max-kanji-continuous-len",
            "no-doubled-joshi" => "style-hint-no-doubled-joshi",
            "no-double-negative-ja" => "style-hint-no-double-negative-ja",
            "ja-no-weak-phrase" => "style-hint-ja-no-weak-phrase",
            "ja-no-redundant-expression" => "style-hint-ja-no-redundant-expression",
            "max-comma" => "style-hint-max-comma",
            "no-exclamation-question-mark" => "style-hint-no-exclamation-question-mark",
            "no-hankaku-kana" => "style-hint-no-hankaku-kana",
            _ => "style-hint-textlint",
        })
    }

    /// A short description for the card pane. Custom rules retain their own copy.
    pub fn style_note_message(
        self, code: &str, rule: Option<&str>, detail: &str, limit: Option<usize>,
    ) -> String {
        let key = match style_rule_name(code, rule) {
            "ai-phrase" => "style-note-ai-phrase",
            "long-sentence" => "style-note-long-sentence",
            "repeated-ending" => "style-note-repeated-ending",
            "comma-density" | "max-comma" => {
                if let Some(limit) = limit {
                    let mut args = FluentArgs::new();
                    args.set("limit", limit);
                    return self.format("style-note-commas-with-limit", &args);
                }
                "style-note-commas"
            }
            "rhetorical-negation" => "style-note-rhetorical-negation",
            "hedging" => "style-note-hedging",
            "no-mix-dearu-desumasu" => "style-note-no-mix-dearu-desumasu",
            "ja-no-mixed-period" => "style-note-ja-no-mixed-period",
            "max-kanji-continuous-len" => "style-note-max-kanji-continuous-len",
            "no-doubled-joshi" => "style-note-no-doubled-joshi",
            "no-double-negative-ja" => "style-note-no-double-negative-ja",
            "ja-no-weak-phrase" => "style-note-ja-no-weak-phrase",
            "ja-no-redundant-expression" => "style-note-ja-no-redundant-expression",
            "no-exclamation-question-mark" => "style-note-no-exclamation-question-mark",
            "no-hankaku-kana" => "style-note-no-hankaku-kana",
            _ => return detail.to_owned(),
        };
        self.text(key).to_owned()
    }

    pub fn style_diagnostic(self, code: &str, rule: Option<&str>, message: &str) -> String {
        let mut args = FluentArgs::new();
        if code == "style/textlint-status" {
            args.set("detail", message);
            return self.format("style-textlint-status", &args);
        }
        args.set("message", message);
        args.set("code", code);
        args.set("hint", self.style_fix_hint(code, rule));
        if let Some(rule) = rule {
            args.set("rule", rule);
            self.format("style-diagnostic-with-alias", &args)
        } else {
            self.format("style-diagnostic", &args)
        }
    }

    fn catalog(self) -> &'static Catalog {
        &CATALOGS[self.0]
    }
}

fn style_rule_name<'a>(code: &'a str, rule: Option<&'a str>) -> &'a str {
    let name = code.strip_prefix("style/").unwrap_or(code);
    let name = if name == "textlint" {
        rule.and_then(|rule| rule.rsplit('/').next()).unwrap_or(name)
    } else {
        name
    };
    match name {
        "sentence-length" => "long-sentence",
        "max-ten" => "comma-density",
        "no-ai-hype-expressions" => "ai-phrase",
        _ => name,
    }
}

struct Catalog {
    bundle: FluentBundle<FluentResource>,
    plain: HashMap<String, String>,
}

static CATALOGS: LazyLock<HashMap<&'static str, Catalog>> = LazyLock::new(|| {
    RESOURCES
        .iter()
        .map(|&(locale, sources)| {
            let mut bundle =
                FluentBundle::new_concurrent(vec![locale.parse().expect("valid locale tag")]);
            bundle.set_use_isolating(false);
            bundle
                .add_function("STATUS_SEPARATOR", |_, _| FluentValue::from(STATUS_SEPARATOR))
                .expect("unique Fluent function");
            let mut keys = Vec::new();
            for source in sources {
                let resource =
                    FluentResource::try_new((*source).to_owned()).expect("valid Fluent resource");
                for entry in resource.entries() {
                    if let fluent_syntax::ast::Entry::Message(message) = entry {
                        keys.push(message.id.name.to_owned());
                    }
                }
                bundle.add_resource(resource).expect("unique Fluent keys");
            }
            let plain = keys
                .into_iter()
                .filter_map(|key| {
                    let value = bundle.get_message(&key)?.value()?;
                    let mut errors = Vec::new();
                    let text = bundle.format_pattern(value, None, &mut errors).into_owned();
                    errors.is_empty().then_some((key, text))
                })
                .collect();
            (locale, Catalog { bundle, plain })
        })
        .collect()
});

#[cfg(test)]
mod tests;
