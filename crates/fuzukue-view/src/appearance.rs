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

impl DesktopColors {
    pub const fn for_dark_mode(dark: bool) -> Self {
        if dark {
            Self {
                background: 0x403034,
                chrome: 0x37292d,
                chat: 0x49373c,
                raised: 0x533f45,
                code: 0x2e2326,
                divider: 0x806970,
                outline: 0xb49ba2,
            }
        } else {
            Self {
                background: 0xe6f1fa,
                chrome: 0xdceaf5,
                chat: 0xf0f6fc,
                raised: 0xffffff,
                code: 0xd7e7f4,
                divider: 0xa69aa0,
                outline: 0x80717b,
            }
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
    Teal,
    Blue,
    Green,
    Purple,
    Pink,
    Orange,
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
    fn desktop_borders_have_contrast_on_adjacent_surfaces_in_both_themes() {
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

        for dark in [false, true] {
            let colors = DesktopColors::for_dark_mode(dark);
            for (border, minimum) in [(colors.divider, 1.5), (colors.outline, 3.)] {
                for surface in [
                    colors.background, colors.chrome, colors.chat, colors.raised, colors.code,
                ] {
                    let (border_l, surface_l) = (luminance(border), luminance(surface));
                    let ratio = (border_l.max(surface_l) + 0.05) / (border_l.min(surface_l) + 0.05);
                    assert!(ratio >= minimum, "#{border:06x} on #{surface:06x}: {ratio:.2}:1");
                }
            }
        }
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
