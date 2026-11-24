use gpui_kit::component::{
    h_flex,
    select::{SelectDelegate, SelectItem, SelectState},
    ActiveTheme as _, IndexPath,
};
use gpui_kit::{div, prelude::*, px, App, Font, FontFallbacks, SharedString, Task, Window};

pub(super) type FontSelect = SelectState<FontOptions>;
const SYSTEM_FONT: &str = ".SystemUIFont";
const UI_FONTS: &[&str] = &[
    "Yu Gothic UI",
    "游ゴシック UI",
    "Meiryo",
    "メイリオ",
    "Hiragino Sans",
    "Noto Sans JP",
    "Noto Sans CJK JP",
    "IPAexGothic",
    SYSTEM_FONT,
];
const READING_FONTS: &[&str] = &[
    "Yu Mincho",
    "游明朝",
    "BIZ UDMincho",
    "BIZ UD明朝",
    "Hiragino Mincho ProN",
    "Noto Serif CJK JP",
    "IPAexMincho",
    SYSTEM_FONT,
];
const CODE_FONTS: &[&str] = &[
    "Cascadia Mono",
    "BIZ UDGothic",
    "BIZ UDゴシック",
    "Consolas",
    "Menlo",
    "DejaVu Sans Mono",
    "Liberation Mono",
    "monospace",
];

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum FontRole {
    Ui,
    Reading,
    Code,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ChoiceKind {
    Automatic,
    Installed,
    Missing,
}

#[derive(Clone)]
pub(super) struct FontChoice {
    value: String,
    kind: ChoiceKind,
}

impl SelectItem for FontChoice {
    type Value = String;

    fn title(&self) -> SharedString {
        match self.kind {
            ChoiceKind::Automatic => tr!("settings-system-font").into(),
            ChoiceKind::Installed => self.value.clone().into(),
            ChoiceKind::Missing => {
                tr!("settings-font-not-installed", name = self.value.as_str()).into()
            }
        }
    }

    fn value(&self) -> &String {
        &self.value
    }

    fn disabled(&self) -> bool {
        self.kind == ChoiceKind::Missing
    }

    fn render(&self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        h_flex()
            .w_full()
            .gap_3()
            .justify_between()
            .child(div().min_w_0().truncate().child(self.title()))
            .when(self.kind == ChoiceKind::Installed, |row| {
                row.child(
                    div()
                        .flex_shrink_0()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .font_family(self.value.clone())
                        .child(tr!("settings-font-sample")),
                )
            })
    }
}

pub(super) struct FontOptions {
    preferred: Vec<FontChoice>,
    other: Vec<FontChoice>,
    matches: [Vec<usize>; 2],
}

impl FontOptions {
    fn group(&self, section: usize) -> usize {
        if self.matches[0].is_empty() && !self.matches[1].is_empty() {
            section + 1
        } else {
            section
        }
    }

    fn choices(&self, section: usize) -> &[FontChoice] {
        if section == 0 {
            &self.preferred
        } else {
            &self.other
        }
    }

    fn filter(&mut self, query: &str) {
        let query = query.trim().to_lowercase();
        self.matches = [&self.preferred, &self.other].map(|choices| {
            choices
                .iter()
                .enumerate()
                .filter(|(_, item)| item.title().to_lowercase().contains(&query))
                .map(|(index, _)| index)
                .collect()
        });
    }
}

impl SelectDelegate for FontOptions {
    type Item = FontChoice;

    fn sections_count(&self, _: &App) -> usize {
        if self.matches.iter().all(|matches| !matches.is_empty()) {
            2
        } else {
            1
        }
    }

    fn items_count(&self, section: usize) -> usize {
        self.matches.get(self.group(section)).map_or(0, Vec::len)
    }

    fn item(&self, ix: IndexPath) -> Option<&FontChoice> {
        let group = self.group(ix.section);
        let index = *self.matches.get(group)?.get(ix.row)?;
        self.choices(group).get(index)
    }

    fn position<V>(&self, value: &V) -> Option<IndexPath>
    where
        FontChoice: SelectItem<Value = V>,
        V: PartialEq,
    {
        (0..self.matches.len())
            .map(|section| (section, self.group(section)))
            .find_map(|(section, group)| {
                let matches = self.matches.get(group)?;
                matches
                    .iter()
                    .position(|&index| self.choices(group)[index].value() == value)
                    .map(|row| IndexPath::new(row).section(section))
            })
    }

    fn perform_search(&mut self, query: &str, _: &mut Window, _: &mut App) -> Task<()> {
        self.filter(query);
        Task::ready(())
    }

    fn render_section_header(
        &self,
        section: usize,
        _: &mut Window,
        cx: &mut App,
    ) -> Option<gpui_kit::AnyElement> {
        (self.group(section) == 1).then(|| {
            div()
                .px_2()
                .py_1()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child(tr!("settings-other-fonts"))
                .into_any_element()
        })
    }
}

struct Family {
    name: String,
    monospace: bool,
}

pub(super) struct FontCatalog(Vec<Family>);

fn monospace(advances: Option<(f32, f32)>) -> bool {
    advances.is_some_and(|(narrow, wide)| narrow > 0.0 && (wide - narrow).abs() < 0.01)
}

fn saved_families(value: &str) -> impl Iterator<Item = &str> {
    value
        .split(',')
        .map(str::trim)
        .filter(|family| !family.is_empty())
}

impl FontCatalog {
    pub(super) fn from_platform(cx: &App) -> Self {
        let text = cx.text_system();
        Self::new(text.all_font_names(), |name| {
            let font = text.resolve_font(&gpui_kit::font(name.to_owned()));
            let size = px(16.0);
            let narrow = text.advance(font, size, 'i').ok()?.width.into();
            let wide = text.advance(font, size, 'W').ok()?.width.into();
            Some((narrow, wide))
        })
    }

    fn new(mut names: Vec<String>, advances: impl Fn(&str) -> Option<(f32, f32)>) -> Self {
        names.retain(|name| !name.is_empty() && name != SYSTEM_FONT);
        names.sort_by_cached_key(|name| (name.to_lowercase(), name.clone()));
        names.dedup_by(|right, left| right.eq_ignore_ascii_case(left));
        Self(
            names
                .into_iter()
                .map(|name| {
                    let monospace = monospace(advances(&name));
                    Family { name, monospace }
                })
                .collect(),
        )
    }

    fn installed(&self, name: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|family| family.name.eq_ignore_ascii_case(name))
            .map(|family| family.name.as_str())
    }

    pub(super) fn options(&self, role: FontRole, saved: &str) -> (FontOptions, Option<IndexPath>) {
        let mut options = FontOptions {
            preferred: vec![FontChoice {
                value: String::new(),
                kind: ChoiceKind::Automatic,
            }],
            other: Vec::new(),
            matches: Default::default(),
        };
        let saved = saved_families(saved)
            .next()
            .filter(|&name| name != SYSTEM_FONT);
        let installed = saved.and_then(|name| self.installed(name));
        if let Some(name) = saved.filter(|_| installed.is_none()) {
            options.preferred.push(FontChoice {
                value: name.into(),
                kind: ChoiceKind::Missing,
            });
        }
        for family in &self.0 {
            let group = if role == FontRole::Code && !family.monospace {
                &mut options.other
            } else {
                &mut options.preferred
            };
            group.push(FontChoice {
                value: family.name.clone(),
                kind: ChoiceKind::Installed,
            });
        }
        options.filter("");
        let selected = options.position(&installed.or(saved).unwrap_or_default().to_owned());
        (options, selected)
    }

    pub(super) fn resolve(&self, role: FontRole, saved: &str) -> Font {
        let defaults = match role {
            FontRole::Ui => UI_FONTS,
            FontRole::Reading => READING_FONTS,
            FontRole::Code => CODE_FONTS,
        };
        let mut chain = Vec::new();
        for name in saved_families(saved).chain(defaults.iter().copied()) {
            let family = self.installed(name).or_else(|| {
                (name == SYSTEM_FONT || (role == FontRole::Code && name == "monospace"))
                    .then_some(name)
            });
            if let Some(family) = family.filter(|family| !chain.contains(family)) {
                chain.push(family);
            }
        }
        let mut families = chain.into_iter();
        let mut font = gpui_kit::font(families.next().unwrap_or(SYSTEM_FONT).to_owned());
        font.fallbacks = Some(FontFallbacks::from_fonts(
            families.map(str::to_owned).collect(),
        ));
        font
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fuzukue_view::appearance::{AppearanceSettings, FontWeightChoice};
    use gpui_kit::{FontWeight, StyleRefinement};
    use gpui_kit::base::text::{CodeTypography, TextViewStyle};

    fn catalog() -> FontCatalog {
        FontCatalog::new(
            [
                "Zeta Mono",
                "Arial",
                "arial",
                "BIZ UDGothic",
                "Cascadia Mono",
                "Symbols",
            ]
            .map(str::to_owned)
            .into(),
            |name| match name {
                "Zeta Mono" | "Cascadia Mono" | "BIZ UDGothic" => Some((8., 8.)),
                "Symbols" => None,
                _ => Some((3., 11.)),
            },
        )
    }

    #[test]
    fn lists_automatic_then_monospace_and_filters_by_family() {
        let (mut options, selected) = catalog().options(FontRole::Code, "ariaL, Zeta Mono");
        assert_eq!(selected, Some(IndexPath::new(0).section(1)));
        let choices = |options: &FontOptions, section: usize| {
            options.matches[section]
                .iter()
                .map(|&index| options.choices(section)[index].value.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            choices(&options, 0),
            ["", "BIZ UDGothic", "Cascadia Mono", "Zeta Mono"]
        );
        assert_eq!(choices(&options, 1), ["Arial", "Symbols"]);
        options.filter("  MoNO ");
        assert_eq!(choices(&options, 0), ["Cascadia Mono", "Zeta Mono"]);
        assert!(choices(&options, 1).is_empty());
        options.filter("SYMBOL");
        assert!(choices(&options, 0).is_empty());
        assert_eq!(choices(&options, 1), ["Symbols"]);
        assert_eq!(options.item(IndexPath::new(0)).unwrap().value(), "Symbols");
        assert_eq!(
            options.position(&"Symbols".to_owned()),
            Some(IndexPath::new(0))
        );
        assert!(!monospace(Some((0., 0.))));
    }

    #[test]
    fn legacy_lists_keep_installed_fallbacks_and_missing_selection() {
        let catalog = catalog();
        let legacy = " Missing , zeta mono, BIZ UDGothic ";
        let (options, selected) = catalog.options(FontRole::Code, legacy);
        let selected = options.item(selected.unwrap()).unwrap();
        assert_eq!(selected.value(), "Missing");
        assert!(selected.disabled());

        let font = catalog.resolve(FontRole::Code, legacy);
        assert_eq!(font.family.as_ref(), "Zeta Mono");
        assert_eq!(
            font.fallbacks.unwrap().fallback_list(),
            ["BIZ UDGothic", "Cascadia Mono", "monospace"]
        );
        let font = catalog.resolve(FontRole::Code, "Missing");
        assert_eq!(font.family.as_ref(), "Cascadia Mono");
        assert_eq!(
            font.fallbacks.unwrap().fallback_list(),
            ["BIZ UDGothic", "monospace"]
        );
        let font = catalog.resolve(FontRole::Ui, "Arial");
        assert_eq!(font.family.as_ref(), "Arial");
        assert_eq!(font.fallbacks.unwrap().fallback_list(), [SYSTEM_FONT]);
    }

    #[test]
    fn role_weights_reach_text_styles_and_preserve_fallbacks() {
        let fonts = catalog();
        let roles = super::super::FONT_ROLES;
        for role in roles {
            for (choice, weight) in [
                (FontWeightChoice::Light, FontWeight::LIGHT),
                (FontWeightChoice::Regular, FontWeight::NORMAL),
                (FontWeightChoice::Medium, FontWeight::MEDIUM),
            ] {
                let mut settings = AppearanceSettings::default();
                role.select(&mut settings, "Arial, Zeta Mono");
                role.select_weight(&mut settings, choice);
                for tested in roles {
                    let font = tested.resolve(&fonts, &settings);
                    let expected = if tested == role { weight } else { FontWeight::NORMAL };
                    let style = StyleRefinement::default().font(font.clone());
                    assert_eq!(style.text.font_weight, Some(expected));
                    if tested == role {
                        assert_eq!(font.family.as_str(), "Arial");
                        assert_eq!(style.text.font_fallbacks.unwrap().fallback_list()[0], "Zeta Mono");
                    }
                    if tested == FontRole::Code {
                        let code = TextViewStyle::default().with_code_typography(&CodeTypography {
                            font,
                            size_scale: settings.code_font_scale(),
                        });
                        assert_eq!(code.inline_code_style().text.font_weight, Some(expected));
                        assert_eq!(code.code_block().text.font_weight, Some(expected));
                    }
                }
            }
        }
    }
}
