use std::sync::RwLock;

use fuzukue_i18n::Locale;

static CURRENT: RwLock<Locale> = RwLock::new(Locale::JAPANESE);

pub(crate) fn current() -> Locale {
    *CURRENT.read().unwrap()
}

pub(crate) fn apply(preference: &str) {
    let locale = if preference == "system" {
        sys_locale::get_locales().find_map(|tag| Locale::supported(&tag))
    } else {
        Locale::supported(preference)
    }
    .unwrap_or(Locale::JAPANESE);
    *CURRENT.write().unwrap() = locale;
    gpui_kit::component::set_locale(locale.tag());
}

macro_rules! tr {
    ($key:expr) => {
        $crate::i18n::current().text($key)
    };
    ($key:expr, $($name:ident = $value:expr),+ $(,)?) => {{
        let mut args = fuzukue_i18n::FluentArgs::new();
        $(args.set(stringify!($name), $value);)+
        $crate::i18n::current().format($key, &args)
    }};
}

/// The server supplies the display text for older clients and for unstructured
/// approvals. A catalog-backed approval can also follow a live locale change.
#[derive(Clone)]
pub(super) struct Message {
    pub fallback: String,
    pub localized: Option<fuzukue_proto::LocalizedMessage>,
}

impl Message {
    pub fn text(&self) -> std::borrow::Cow<'_, str> {
        if let Some(message) = &self.localized {
            let mut args = fuzukue_i18n::FluentArgs::new();
            for (name, value) in &message.args {
                args.set(name.as_str(), value.as_str());
            }
            let text = current().format(&message.key, &args);
            if text != message.key {
                return text.into();
            }
        }
        self.fallback.as_str().into()
    }
}
