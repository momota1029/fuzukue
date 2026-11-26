//! クライアントの外観設定。保存形式と値の範囲をここで扱う。

use std::collections::BTreeMap;

use serde::{Deserialize, Deserializer, Serialize};

pub const UI_FONT_SIZE: std::ops::RangeInclusive<f32> = 12.0..=24.0;
pub const READING_FONT_SIZE: std::ops::RangeInclusive<f32> = 12.0..=36.0;
pub const CODE_FONT_SIZE_PERCENT: std::ops::RangeInclusive<f32> = 80.0..=110.0;
pub const DEFAULT_CODE_FONT_SIZE_PERCENT: f32 = 92.0;
pub const READING_LINE_HEIGHT: std::ops::RangeInclusive<f32> = 1.2..=2.4;
pub const PAPER_CHAT_WIDTH: std::ops::RangeInclusive<f32> = 280.0..=640.0;
pub const SIDEBAR_WIDTH: std::ops::RangeInclusive<f32> = 168.0..=420.0;
pub const MATH_SCALE: std::ops::RangeInclusive<f32> = 0.75..=2.0;

/// Desktop surfaces and their opaque borders, in sRGB.
pub struct DesktopColors {
    pub background: u32,
    pub chrome: u32,
    pub chat: u32,
    pub raised: u32,
    pub code: u32,
    /// Decorative rules target at least 1.5:1 on the desktop surfaces.
    pub divider: u32,
    /// Card and control outlines target at least 3:1 on the desktop surfaces.
    pub outline: u32,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemePreset {
    Parchment,
    Iris,
    Monochrome,
    Ink,
    Fern,
    Fjord,
    Terracotta,
    Rose,
    Lagoon,
    #[default]
    #[serde(other)]
    Classic,
}

impl ThemePreset {
    pub const ALL: [Self; 10] = [
        Self::Classic,
        Self::Parchment,
        Self::Iris,
        Self::Monochrome,
        Self::Ink,
        Self::Fern,
        Self::Fjord,
        Self::Terracotta,
        Self::Rose,
        Self::Lagoon,
    ];

    pub const fn is_monochrome(self) -> bool {
        matches!(self, Self::Monochrome | Self::Ink)
    }

    pub const fn accent(self) -> AccentPreset {
        match self {
            Self::Classic | Self::Lagoon => AccentPreset::Teal,
            Self::Parchment | Self::Terracotta => AccentPreset::Orange,
            Self::Iris => AccentPreset::Purple,
            Self::Monochrome | Self::Ink => AccentPreset::Gray,
            Self::Fern => AccentPreset::Green,
            Self::Fjord => AccentPreset::Blue,
            Self::Rose => AccentPreset::Pink,
        }
    }
}

impl DesktopColors {
    pub const fn for_preset(preset: ThemePreset, dark: bool) -> Self {
        use ThemePreset::*;

        match (preset, dark) {
            (Parchment, false) => Self {
                background: 0xf7efd9,
                chrome: 0xede1c4,
                chat: 0xfbf5e8,
                raised: 0xfffcf5,
                code: 0xeee1c7,
                divider: 0xb39e79,
                outline: 0x806e51,
            },
            (Parchment, true) => Self {
                background: 0x2e251c,
                chrome: 0x241b13,
                chat: 0x382d21,
                raised: 0x46392b,
                code: 0x201913,
                divider: 0x79654c,
                outline: 0xab9676,
            },
            (Iris, false) => Self {
                background: 0xf0eaf8,
                chrome: 0xe8def4,
                chat: 0xf6f2fc,
                raised: 0xfdfaff,
                code: 0xe6dcf2,
                divider: 0xa898bd,
                outline: 0x79668d,
            },
            (Iris, true) => Self {
                background: 0x29243b,
                chrome: 0x211c31,
                chat: 0x322b47,
                raised: 0x3e3555,
                code: 0x1c182b,
                divider: 0x746488,
                outline: 0xa492bd,
            },
            (Monochrome, false) => Self {
                background: 0xeeeeee,
                chrome: 0xe4e4e4,
                chat: 0xf4f4f4,
                raised: 0xfafafa,
                code: 0xdddddd,
                divider: 0xaaaaaa,
                outline: 0x747474,
            },
            (Monochrome, true) => Self {
                background: 0x252525,
                chrome: 0x1d1d1d,
                chat: 0x2d2d2d,
                raised: 0x373737,
                code: 0x212121,
                divider: 0x696969,
                outline: 0x9c9c9c,
            },
            (Ink, false) => Self {
                background: 0xfafafa,
                chrome: 0xf2f2f2,
                chat: 0xfdfdfd,
                raised: 0xffffff,
                code: 0xededed,
                divider: 0x929292,
                outline: 0x444444,
            },
            (Ink, true) => Self {
                background: 0x121212,
                chrome: 0x0b0b0b,
                chat: 0x1b1b1b,
                raised: 0x252525,
                code: 0x0e0e0e,
                divider: 0x777777,
                outline: 0xbdbdbd,
            },
            (Fern, false) => Self {
                background: 0xe6f0df,
                chrome: 0xd9e8d0,
                chat: 0xf0f6e9,
                raised: 0xf8fbf3,
                code: 0xdcead7,
                divider: 0x9aac93,
                outline: 0x657d5e,
            },
            (Fern, true) => Self {
                background: 0x222e23,
                chrome: 0x1a241b,
                chat: 0x2a382b,
                raised: 0x334434,
                code: 0x1a261c,
                divider: 0x617460,
                outline: 0x95ac91,
            },
            (Fjord, false) => Self {
                background: 0xe4ecf8,
                chrome: 0xd7e4f5,
                chat: 0xedf3fc,
                raised: 0xf7faff,
                code: 0xdce7f8,
                divider: 0x94a8c0,
                outline: 0x5f7694,
            },
            (Fjord, true) => Self {
                background: 0x1e2a3b,
                chrome: 0x172132,
                chat: 0x26354a,
                raised: 0x2e4057,
                code: 0x192334,
                divider: 0x5c7291,
                outline: 0x92a8c7,
            },
            (Terracotta, false) => Self {
                background: 0xf6e7df,
                chrome: 0xeed9ce,
                chat: 0xfbf0e9,
                raised: 0xfff8f4,
                code: 0xeedcd1,
                divider: 0xb79b90,
                outline: 0x8d6b5d,
            },
            (Terracotta, true) => Self {
                background: 0x32241f,
                chrome: 0x271b18,
                chat: 0x3b2b25,
                raised: 0x48352d,
                code: 0x231b17,
                divider: 0x886458,
                outline: 0xc09a8b,
            },
            (Rose, false) => Self {
                background: 0xf4e5ee,
                chrome: 0xeed8e5,
                chat: 0xfaf0f6,
                raised: 0xfff8fc,
                code: 0xf0dfe9,
                divider: 0xb297aa,
                outline: 0x896780,
            },
            (Rose, true) => Self {
                background: 0x302330,
                chrome: 0x271a28,
                chat: 0x3a2a39,
                raised: 0x473345,
                code: 0x211822,
                divider: 0x806480,
                outline: 0xb89ab4,
            },
            (Lagoon, false) => Self {
                background: 0xe1f0ed,
                chrome: 0xd3e7e3,
                chat: 0xeef7f4,
                raised: 0xf7fcfa,
                code: 0xd9ebe7,
                divider: 0x90aaa3,
                outline: 0x5d7c74,
            },
            (Lagoon, true) => Self {
                background: 0x1e302e,
                chrome: 0x172624,
                chat: 0x263c39,
                raised: 0x2e4944,
                code: 0x172724,
                divider: 0x5f7971,
                outline: 0x8faea5,
            },
            (Classic, true) =>
            Self {
                background: 0x403034,
                chrome: 0x37292d,
                chat: 0x49373c,
                raised: 0x533f45,
                code: 0x2e2326,
                divider: 0x806970,
                outline: 0xb49ba2,
            },
            (Classic, false) => Self {
                background: 0xe6f1fa,
                chrome: 0xdceaf5,
                chat: 0xf0f6fc,
                raised: 0xffffff,
                code: 0xd7e7f4,
                divider: 0xa69aa0,
                outline: 0x80717b,
            },
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemePreference {
    #[default]
    System,
    Light,
    Dark,
}

impl ThemePreference {
    /// スクリーンショット用の一時指定。OS 追従は保存された設定で選ぶ。
    pub fn screenshot_override(value: &str) -> Option<Self> {
        match value {
            "light" => Some(Self::Light),
            "dark" => Some(Self::Dark),
            _ => None,
        }
    }
}

/// Base font weight for an appearance font role.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FontWeightChoice {
    Light,
    #[default]
    Regular,
    Medium,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AccentPreset {
    #[default]
    Theme,
    Teal,
    Blue,
    Green,
    Purple,
    Pink,
    Orange,
    Gray,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AccentChoice {
    Preset(AccentPreset),
    Custom(String),
}

impl Default for AccentChoice {
    fn default() -> Self {
        Self::Preset(AccentPreset::default())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Theme {
    pub base: ThemePreference,
    pub preset: ThemePreset,
    pub accent: AccentChoice,
    /// Font family for Markdown code. Older settings may contain a comma-separated list.
    pub code_font: String,
    pub overrides: BTreeMap<String, String>,
}

fn deserialize_theme<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Theme, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum StoredTheme {
        Legacy(ThemePreference),
        Current(Theme),
    }

    Ok(match StoredTheme::deserialize(deserializer)? {
        StoredTheme::Legacy(base) => Theme {
            base,
            ..Theme::default()
        },
        StoredTheme::Current(theme) => theme,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct MainWindowPlacement {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    #[serde(default)]
    pub maximized: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppearanceSettings {
    #[serde(deserialize_with = "deserialize_theme")]
    pub theme: Theme,
    pub locale: String,
    pub ui_font: String,
    pub ui_font_weight: FontWeightChoice,
    pub ui_font_size: f32,
    pub reading_font: String,
    pub reading_font_weight: FontWeightChoice,
    pub reading_font_size: f32,
    pub code_font: String,
    pub code_font_weight: FontWeightChoice,
    pub code_font_size_percent: f32,
    pub reading_line_height: f32,
    pub paper_font_size: f32,
    pub paper_line_height: f32,
    pub math_scale: f32,
    pub paper_chat_width: f32,
    pub sidebar_width: f32,
    pub thread_info_open: bool,
    pub comments_to_side_thread: bool,
    pub screenshot_without_prompt: bool,
    pub save_server_on_close: bool,
    pub show_worker_messages: bool,
    pub main_window: Option<MainWindowPlacement>,
    pub server: crate::server::ServerSettings,
}

impl Default for AppearanceSettings {
    fn default() -> Self {
        Self {
            theme: Theme::default(),
            locale: "ja".into(),
            ui_font: String::new(),
            ui_font_weight: FontWeightChoice::default(),
            ui_font_size: 16.0,
            reading_font: String::new(),
            reading_font_weight: FontWeightChoice::default(),
            reading_font_size: 16.0,
            code_font: String::new(),
            code_font_weight: FontWeightChoice::default(),
            code_font_size_percent: DEFAULT_CODE_FONT_SIZE_PERCENT,
            reading_line_height: 1.6,
            paper_font_size: 18.0,
            paper_line_height: 1.8,
            math_scale: 1.0,
            paper_chat_width: 420.0,
            sidebar_width: 224.0,
            thread_info_open: false,
            comments_to_side_thread: false,
            screenshot_without_prompt: false,
            save_server_on_close: false,
            show_worker_messages: false,
            main_window: None,
            server: Default::default(),
        }
    }
}

impl AppearanceSettings {
    pub fn from_toml(text: &str) -> Result<Self, toml::de::Error> {
        let mut settings: Self = toml::from_str(text)?;
        settings.normalize();
        Ok(settings)
    }

    pub fn to_toml(&self) -> Result<String, toml::ser::Error> {
        let mut settings = self.clone();
        settings.normalize();
        toml::to_string_pretty(&settings)
    }

    pub fn normalize(&mut self) {
        let defaults = Self::default();
        for font in [
            &mut self.ui_font,
            &mut self.reading_font,
            &mut self.code_font,
            &mut self.theme.code_font,
        ] {
            let trimmed = font.trim();
            if trimmed.len() != font.len() {
                *font = trimmed.to_owned();
            }
        }
        for (value, range, default) in [
            (&mut self.ui_font_size, UI_FONT_SIZE, defaults.ui_font_size),
            (
                &mut self.reading_font_size,
                READING_FONT_SIZE,
                defaults.reading_font_size,
            ),
            (
                &mut self.code_font_size_percent,
                CODE_FONT_SIZE_PERCENT,
                defaults.code_font_size_percent,
            ),
            (
                &mut self.reading_line_height,
                READING_LINE_HEIGHT,
                defaults.reading_line_height,
            ),
            (
                &mut self.paper_font_size,
                READING_FONT_SIZE,
                defaults.paper_font_size,
            ),
            (
                &mut self.paper_line_height,
                READING_LINE_HEIGHT,
                defaults.paper_line_height,
            ),
            (&mut self.math_scale, MATH_SCALE, defaults.math_scale),
            (&mut self.paper_chat_width, PAPER_CHAT_WIDTH, defaults.paper_chat_width),
            (&mut self.sidebar_width, SIDEBAR_WIDTH, defaults.sidebar_width),
        ] {
            *value = if value.is_finite() {
                value.clamp(*range.start(), *range.end())
            } else {
                default
            };
        }
        self.main_window = self.main_window.filter(|window| {
            [window.x, window.y, window.width, window.height]
                .into_iter()
                .all(f32::is_finite)
                && window.width > 0.0
                && window.height > 0.0
        });
    }

    pub fn code_font_scale(&self) -> f32 {
        code_font_scale(self.code_font_size_percent)
    }
}

/// Resolve the code size multiplier from the persisted percentage.
pub fn code_font_scale(percent: f32) -> f32 {
    let percent = if percent.is_finite() {
        percent.clamp(*CODE_FONT_SIZE_PERCENT.start(), *CODE_FONT_SIZE_PERCENT.end())
    } else {
        DEFAULT_CODE_FONT_SIZE_PERCENT
    };
    percent / 100.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desktop_borders_have_contrast_on_adjacent_surfaces_in_every_preset_and_mode() {
        fn luminance(color: u32) -> f64 {
            let linear = |shift: u32| {
                let channel = f64::from((color >> shift) & 0xff) / 255.;
                if channel <= 0.04045 {
                    channel / 12.92
                } else {
                    ((channel + 0.055) / 1.055).powf(2.4)
                }
            };
            0.2126 * linear(16) + 0.7152 * linear(8) + 0.0722 * linear(0)
        }

        for preset in ThemePreset::ALL {
            for dark in [false, true] {
                let colors = DesktopColors::for_preset(preset, dark);
                for (border, minimum) in [(colors.divider, 1.5), (colors.outline, 3.)] {
                    for surface in [
                        colors.background, colors.chrome, colors.chat, colors.raised, colors.code,
                    ] {
                        let (border_l, surface_l) = (luminance(border), luminance(surface));
                        let ratio = (border_l.max(surface_l) + 0.05) / (border_l.min(surface_l) + 0.05);
                        assert!(
                            ratio >= minimum,
                            "{preset:?} dark={dark}: #{border:06x} on #{surface:06x}: {ratio:.2}:1",
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn every_theme_preset_round_trips_with_its_saved_id() {
        for (preset, id) in [
            (ThemePreset::Classic, "classic"),
            (ThemePreset::Parchment, "parchment"),
            (ThemePreset::Iris, "iris"),
            (ThemePreset::Monochrome, "monochrome"),
            (ThemePreset::Ink, "ink"),
            (ThemePreset::Fern, "fern"),
            (ThemePreset::Fjord, "fjord"),
            (ThemePreset::Terracotta, "terracotta"),
            (ThemePreset::Rose, "rose"),
            (ThemePreset::Lagoon, "lagoon"),
        ] {
            let settings = AppearanceSettings::from_toml(&format!("[theme]\npreset = '{id}'")).unwrap();
            assert_eq!(settings.theme.preset, preset);
            let written = settings.to_toml().unwrap();
            assert!(written.contains(&format!("preset = \"{id}\"")));
            assert_eq!(AppearanceSettings::from_toml(&written).unwrap(), settings);
        }
    }

    #[test]
    fn unknown_theme_preset_falls_back_without_losing_other_settings() {
        let settings = AppearanceSettings::from_toml(
            "locale = 'en'\n[theme]\npreset = 'future-palette'\nbase = 'dark'\naccent = '#2468ac'",
        ).unwrap();
        assert_eq!(settings.locale, "en");
        assert_eq!(settings.theme.preset, ThemePreset::Classic);
        assert_eq!(settings.theme.base, ThemePreference::Dark);
        assert_eq!(settings.theme.accent, AccentChoice::Custom("#2468ac".into()));
    }

    #[test]
    fn partial_settings_fill_defaults_and_ignore_unknown_keys() {
        let settings = AppearanceSettings::from_toml("theme = 'dark'\nfuture_key = true").unwrap();
        assert_eq!(
            settings,
            AppearanceSettings {
                theme: Theme {
                    base: ThemePreference::Dark,
                    ..Theme::default()
                },
                ..AppearanceSettings::default()
            }
        );
    }

    #[test]
    fn worker_messages_default_to_folded_and_can_be_enabled() {
        let settings = AppearanceSettings::from_toml("locale = 'ja'").unwrap();
        assert!(!settings.show_worker_messages);

        let settings = AppearanceSettings::from_toml("show_worker_messages = true").unwrap();
        assert!(settings.show_worker_messages);
        assert!(AppearanceSettings::from_toml(&settings.to_toml().unwrap())
            .unwrap().show_worker_messages);
    }

    #[test]
    fn language_round_trips_with_the_appearance_settings() {
        let settings = AppearanceSettings::from_toml("locale = 'en'").unwrap();
        assert_eq!(settings.locale, "en");
        assert_eq!(
            AppearanceSettings::from_toml(&settings.to_toml().unwrap()).unwrap().locale,
            "en",
        );
    }

    #[test]
    fn clamps_numbers_and_restores_defaults_for_non_finite_values() {
        let settings = AppearanceSettings::from_toml(
            "ui_font_size = 1\nreading_font_size = 100\nreading_line_height = inf\nmath_scale = nan\nsidebar_width = 500.0",
        )
        .unwrap();
        assert_eq!(&settings.ui_font_size, UI_FONT_SIZE.start());
        assert_eq!(&settings.reading_font_size, READING_FONT_SIZE.end());
        assert_eq!(&settings.sidebar_width, SIDEBAR_WIDTH.end());
        assert_eq!(
            settings.reading_line_height,
            AppearanceSettings::default().reading_line_height
        );
        assert_eq!(
            settings.math_scale,
            AppearanceSettings::default().math_scale
        );
        assert_eq!(
            AppearanceSettings::from_toml("reading_line_height = 0\nmath_scale = 3")
                .unwrap()
                .math_scale,
            2.0
        );
    }

    #[test]
    fn normalizes_font_names_and_round_trips() {
        let settings = AppearanceSettings::from_toml(
            "theme = 'light'\nui_font = '  '\nreading_font = ' ＭＳ 明朝 '\ncode_font = 'Consolas'\nmath_scale = 1.25\npaper_chat_width = 455.0\nsidebar_width = 315.0",
        )
        .unwrap();
        assert_eq!(settings.ui_font, AppearanceSettings::default().ui_font);
        assert_eq!(settings.reading_font, "ＭＳ 明朝");
        assert_eq!(settings.paper_chat_width, 455.0);
        assert_eq!(settings.sidebar_width, 315.0);
        let serialized = settings.to_toml().unwrap();
        assert_eq!(
            AppearanceSettings::from_toml(&serialized).unwrap(),
            settings
        );
    }

    #[test]
    fn legacy_font_lists_and_code_size_round_trip() {
        let old = "ui_font = 'Yu Gothic UI, Meiryo'\nreading_font = 'Yu Mincho, Noto Serif CJK JP'\ncode_font = 'Menlo, BIZ UDGothic'\n[theme]\ncode_font = 'Consolas, Meiryo'";
        let mut settings = AppearanceSettings::from_toml(old).unwrap();
        assert_eq!(settings.ui_font, "Yu Gothic UI, Meiryo");
        assert_eq!(settings.reading_font, "Yu Mincho, Noto Serif CJK JP");
        assert_eq!(settings.code_font, "Menlo, BIZ UDGothic");
        assert_eq!(settings.theme.code_font, "Consolas, Meiryo");
        assert_eq!(settings.code_font_scale(), 0.92);
        settings.code_font_size_percent = 107.0;
        assert_eq!(AppearanceSettings::from_toml(&settings.to_toml().unwrap()).unwrap(), settings);
    }

    #[test]
    fn code_scale_applies_to_the_surrounding_size() {
        assert_eq!(code_font_scale(92.0) * 20.0, 18.4);
        assert_eq!(code_font_scale(79.0) * 20.0, 16.0);
        assert_eq!(code_font_scale(111.0) * 20.0, 22.0);
        assert_eq!(code_font_scale(f32::NAN), 0.92);
    }

    #[test]
    fn paper_typography_has_independent_defaults_and_limits() {
        let settings = AppearanceSettings::from_toml(
            "reading_font_size = 14\nreading_line_height = 1.4",
        )
        .unwrap();
        assert_eq!(settings.paper_font_size, 18.0);
        assert_eq!(settings.paper_line_height, 1.8);

        let settings = AppearanceSettings::from_toml("paper_font_size = 99\npaper_line_height = 0")
            .unwrap();
        assert_eq!(settings.paper_font_size, *READING_FONT_SIZE.end());
        assert_eq!(settings.paper_line_height, *READING_LINE_HEIGHT.start());
        assert_eq!(AppearanceSettings::from_toml(&settings.to_toml().unwrap()).unwrap(), settings);
    }

    #[test]
    fn rejects_invalid_toml_and_unknown_modes() {
        assert!(AppearanceSettings::from_toml("[").is_err());
        assert!(AppearanceSettings::from_toml("theme = 'blue'").is_err());
    }

    #[test]
    fn main_window_placement_round_trips_and_discards_invalid_bounds() {
        let placement = "\n[main_window]\nx = -1200.0\ny = 40.0\nwidth = 1100.0\nheight = 720.0\nmaximized = true";
        let settings = AppearanceSettings::from_toml(placement).unwrap();
        assert!(settings.main_window.unwrap().maximized);
        assert_eq!(
            AppearanceSettings::from_toml(&settings.to_toml().unwrap()).unwrap(),
            settings,
        );
        let invalid = AppearanceSettings::from_toml(&placement.replace("1100.0", "nan")).unwrap();
        assert!(invalid.main_window.is_none());
    }

    #[test]
    fn screenshot_override_only_accepts_fixed_modes() {
        assert_eq!(
            ThemePreference::screenshot_override("dark"),
            Some(ThemePreference::Dark)
        );
        assert_eq!(
            ThemePreference::screenshot_override("light"),
            Some(ThemePreference::Light)
        );
        assert_eq!(ThemePreference::screenshot_override("system"), None);
        assert_eq!(ThemePreference::screenshot_override(""), None);
    }
}
