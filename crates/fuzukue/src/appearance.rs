//! 外観とサーバーの設定、設定ペイン。

use std::{
    cell::Cell,
    fs,
    path::{Path, PathBuf},
    rc::Rc,
    time::Duration,
};

use gpui_kit::base::text::CodeTypography;
use gpui_kit::component::{
    button::{Button, ButtonCustomVariant, ButtonVariants as _},
    h_flex,
    input::{Input, InputEvent, InputState},
    scroll::{ScrollableElement as _, ScrollbarMode},
    select::{Select, SelectEvent, SelectState},
    v_flex, ActiveTheme as _, Disableable as _, Sizable as _, Theme, ThemeMode,
};
use gpui_kit::{
    div, point, prelude::*, px, relative, rgb, size, App, Bounds, Context, DefiniteLength,
    DisplayId, Empty, Entity, Font, FontWeight, Hsla, Pixels, Rgba, SharedString,
    StyleRefinement, Task, Window,
    WindowBounds, WindowOptions,
};
use gpui_pdf::PdfStyle;
use fuzukue_view::appearance::{
    AccentChoice, AccentPreset, AppearanceSettings, DesktopColors, FontWeightChoice, MainWindowPlacement,
    ThemePreference, CODE_FONT_SIZE_PERCENT, MATH_SCALE, READING_FONT_SIZE, READING_LINE_HEIGHT,
    UI_FONT_SIZE,
};
use fuzukue_view::server::{ServerMode, ServerSettings};

use super::{conn::Target, icons::Symbol, math, Workbench};
use crate::pane_resize::PANE_INSET;

mod font_picker;
use font_picker::{FontCatalog, FontOptions, FontRole, FontSelect};

// Leave room for descenders and fallback glyphs at every UI font size.
pub(super) const UI_LINE_HEIGHT: DefiniteLength = relative(1.6);

pub(super) fn settings_switch(
    id: impl Into<gpui_kit::ElementId>,
    label: &str,
    checked: bool,
    disabled: bool,
    cx: &App,
    on_change: impl Fn(&bool, &mut Window, &mut App) + 'static,
) -> gpui_kit::base::Switch {
    use gpui_kit::base::{Switch, SwitchThumb, SwitchTrack};

    let theme = cx.theme();
    let id = id.into();
    let background = if checked { theme.tokens.primary } else { theme.tokens.switch };
    let thumb = SwitchThumb::new(checked).size(px(16.)).rounded_full().bg(theme.tokens.switch_thumb);
    Switch::new(id.clone())
        .checked(checked)
        .disabled(disabled)
        .accessibility_label(label.to_owned())
        .on_change(move |next, _, window, cx| on_change(&next, window, cx))
        .flex()
        .items_center()
        .justify_between()
        .gap_2()
        .min_h(px(28.))
        .px_1()
        .rounded_md()
        .border_1()
        .border_color(theme.transparent)
        .focus_visible(|style| style.border_color(theme.ring))
        .when(disabled, |row| row.text_color(theme.muted_foreground))
        .when(!disabled, |row| row.cursor_pointer())
        .child(div().min_w_0().child(label.to_owned()))
        .child(
            SwitchTrack::new((id, "track"))
                .flex()
                .items_center()
                .when(checked, |track| track.justify_end())
                .w(px(36.))
                .h(px(20.))
                .px(px(2.))
                .flex_shrink_0()
                .rounded_full()
                .bg(if disabled { background.opacity(0.5) } else { *background })
                .child(thumb),
        )
}

#[derive(Default)]
pub(super) struct SaveServerOnClose(pub bool);
impl gpui_kit::Global for SaveServerOnClose {}

const THEMES: [(ThemePreference, &str, &str); 3] = [
    (ThemePreference::System, "theme-system", "settings-follow-system"),
    (ThemePreference::Light, "theme-light", "settings-light"),
    (ThemePreference::Dark, "theme-dark", "settings-dark"),
];

pub(super) struct ThemeSurfaces {
    background: Hsla,
    chrome: Hsla,
    divider: Hsla,
    outline: Hsla,
    pub(super) chat: Hsla,
    pub(super) code: Hsla,
}

impl ThemeSurfaces {
    pub(super) fn for_mode(mode: ThemeMode) -> Self {
        let colors = DesktopColors::for_dark_mode(mode.is_dark());
        let [background, chrome, divider, outline, chat, code] =
            [colors.background, colors.chrome, colors.divider, colors.outline, colors.chat, colors.code]
                .map(|color| rgb(color).into());
        Self {
            background,
            chrome,
            divider,
            outline,
            chat,
            code,
        }
    }

    pub(super) fn composer_disabled(&self, dark: bool) -> (Hsla, Hsla) {
        let fill = with_lightness(self.chat, self.chat.l + if dark { 0.14 } else { -0.09 });
        let icon = with_lightness(self.chat, if dark { 0.64 } else { 0.43 });
        (fill, icon)
    }

    pub(super) fn composer_stop(&self, dark: bool) -> (Hsla, Hsla) {
        (with_lightness(self.chat, if dark { 0.85 } else { 0.38 }), self.chat)
    }
}

struct AccentSwatch {
    preset: AccentPreset,
    label: &'static str,
    light: u32,
    dark: u32,
}

impl AccentSwatch {
    fn color(&self, dark: bool) -> Hsla {
        rgb(if dark { self.dark } else { self.light }).into()
    }
}

const ACCENT_SWATCHES: [AccentSwatch; 6] = [
    AccentSwatch {
        preset: AccentPreset::Teal,
        label: "settings-accent-teal",
        light: 0x246c72,
        dark: 0x309ea4,
    },
    AccentSwatch {
        preset: AccentPreset::Blue,
        label: "settings-accent-blue",
        light: 0x2e63ac,
        dark: 0x639dea,
    },
    AccentSwatch {
        preset: AccentPreset::Green,
        label: "settings-accent-green",
        light: 0x326c43,
        dark: 0x5ba974,
    },
    AccentSwatch {
        preset: AccentPreset::Purple,
        label: "settings-accent-purple",
        light: 0x7046a2,
        dark: 0xb78be6,
    },
    AccentSwatch {
        preset: AccentPreset::Pink,
        label: "settings-accent-pink",
        light: 0x9b3c65,
        dark: 0xe080ac,
    },
    AccentSwatch {
        preset: AccentPreset::Orange,
        label: "settings-accent-orange",
        light: 0x924e21,
        dark: 0xdf935a,
    },
];

fn parse_hex_color(text: &str) -> Option<u32> {
    let digits = text.trim().strip_prefix('#')?;
    if !matches!(digits.len(), 3 | 6) || !digits.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let value = u32::from_str_radix(digits, 16).ok()?;
    Some(if digits.len() == 3 {
        let red = (value >> 8) * 0x11;
        let green = ((value >> 4) & 0xf) * 0x11;
        let blue = (value & 0xf) * 0x11;
        (red << 16) | (green << 8) | blue
    } else {
        value
    })
}

fn luminance(color: Hsla) -> f32 {
    let Rgba { r, g, b, .. } = color.into();
    let linear = |channel: f32| {
        if channel <= 0.04045 {
            channel / 12.92
        } else {
            ((channel + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * linear(r) + 0.7152 * linear(g) + 0.0722 * linear(b)
}

fn contrast_ratio(first: Hsla, second: Hsla) -> f32 {
    let (first, second) = (luminance(first), luminance(second));
    (first.max(second) + 0.05) / (first.min(second) + 0.05)
}

fn button_foreground(dark: bool) -> Hsla {
    rgb(if dark { 0x0a0a0a } else { 0xffffff }).into()
}

fn with_lightness(color: Hsla, lightness: f32) -> Hsla {
    Hsla {
        l: lightness.clamp(0., 1.),
        ..color
    }
}

pub(super) fn style_note_warning(warning: Hsla, background: Hsla) -> Hsla {
    const MIN_CONTRAST: f32 = 3.2;
    let mut warning = Hsla { s: warning.s * 0.7, a: 1., ..warning };
    if contrast_ratio(warning, background) < MIN_CONTRAST {
        let extreme = if luminance(background) > 0.18 { 0. } else { 1. };
        let mut low = 0.;
        let mut high = 1.;
        for _ in 0..16 {
            let amount = (low + high) / 2.;
            let candidate = with_lightness(warning, warning.l + (extreme - warning.l) * amount);
            if contrast_ratio(candidate, background) >= MIN_CONTRAST {
                high = amount;
            } else {
                low = amount;
            }
        }
        warning = with_lightness(warning, warning.l + (extreme - warning.l) * high);
    }
    let mut low = 0.;
    let mut high = 1.;
    for _ in 0..16 {
        let opacity = (low + high) / 2.;
        if contrast_ratio(background.blend(warning.opacity(opacity)), background) >= MIN_CONTRAST {
            high = opacity;
        } else {
            low = opacity;
        }
    }
    background.blend(warning.opacity(high))
}

pub(super) fn editor_style_note_warning(theme: &Theme) -> Hsla {
    let style = &theme.highlight_theme.style;
    let surface = theme.background.blend(
        style.editor_background.unwrap_or_else(|| theme.input_background()),
    );
    let active = surface.blend(style.editor_active_line.unwrap_or(theme.transparent));
    let gutter = surface.blend(style.editor_gutter_background.unwrap_or(theme.transparent));
    let closest = [surface, active, gutter].into_iter().max_by(|a, b| {
        let order = luminance(*a).total_cmp(&luminance(*b));
        if theme.is_dark() { order } else { order.reverse() }
    }).unwrap();
    style_note_warning(theme.warning, closest)
}

fn accessible_accent(color: Hsla, surface: Hsla, dark: bool, surface_ratio: f32) -> Hsla {
    let foreground = button_foreground(dark);
    let readable = |color| {
        contrast_ratio(color, foreground) >= 4.5 && contrast_ratio(color, surface) >= surface_ratio
    };
    if readable(color) {
        return color;
    }
    // Keep the hue and saturation while moving just far enough toward white or black.
    let mut passing = if dark { 1. } else { 0. };
    let mut failing = color.l;
    for _ in 0..24 {
        let midpoint = (passing + failing) / 2.;
        if readable(with_lightness(color, midpoint)) {
            passing = midpoint;
        } else {
            failing = midpoint;
        }
    }
    with_lightness(color, passing)
}

fn accent_color(choice: &AccentChoice, surface: Hsla, dark: bool) -> Hsla {
    let color = match choice {
        AccentChoice::Preset(preset) => ACCENT_SWATCHES
            .iter()
            .find(|swatch| swatch.preset == *preset)
            .unwrap_or(&ACCENT_SWATCHES[0])
            .color(dark),
        AccentChoice::Custom(value) => parse_hex_color(value)
            .map_or_else(|| ACCENT_SWATCHES[0].color(dark), |hex| rgb(hex).into()),
    };
    // Secondary surfaces are more demanding than the composer and user bubbles.
    accessible_accent(color, surface, dark, 4.5)
}

fn accent_button_states(accent: Hsla, surface: Hsla, dark: bool) -> [Hsla; 3] {
    let muted_lightness = if dark { accent.l - 0.03 } else { accent.l + 0.03 };
    let muted = Hsla {
        s: accent.s * 0.55,
        ..with_lightness(accent, muted_lightness)
    };
    [
        with_lightness(accent, accent.l + 0.04),
        with_lightness(accent, accent.l - 0.04),
        muted,
    ]
    .map(|color| accessible_accent(color, surface, dark, 3.))
}

pub(super) fn accent_button(button: Button, enabled: bool, cx: &App) -> Button {
    let theme = cx.theme();
    let accent = theme.link;
    let dark = theme.is_dark();
    let foreground = button_foreground(dark);
    let [hover, pressed, disabled] = accent_button_states(accent, theme.secondary, dark);
    button
        .custom(
            ButtonCustomVariant::new(cx)
                .color(accent)
                .foreground(foreground)
                .hover(hover)
                .active(pressed),
        )
        .bg(if enabled { accent } else { disabled })
        .text_color(foreground)
        .disabled(!enabled)
}

const FONT_ROLES: [FontRole; 3] = [FontRole::Ui, FontRole::Reading, FontRole::Code];

impl FontRole {
    fn weight(self, settings: &AppearanceSettings) -> FontWeightChoice {
        match self {
            Self::Ui => settings.ui_font_weight,
            Self::Reading => settings.reading_font_weight,
            Self::Code => settings.code_font_weight,
        }
    }

    fn select_weight(self, settings: &mut AppearanceSettings, weight: FontWeightChoice) {
        match self {
            Self::Ui => settings.ui_font_weight = weight,
            Self::Reading => settings.reading_font_weight = weight,
            Self::Code => settings.code_font_weight = weight,
        }
    }

    fn saved(self, settings: &AppearanceSettings) -> &str {
        match self {
            Self::Ui => &settings.ui_font,
            Self::Reading => &settings.reading_font,
            Self::Code if !settings.theme.code_font.is_empty() => &settings.theme.code_font,
            Self::Code => &settings.code_font,
        }
    }

    fn resolve(self, fonts: &FontCatalog, settings: &AppearanceSettings) -> Font {
        let mut font = fonts.resolve(self, self.saved(settings));
        font.weight = match self.weight(settings) {
            FontWeightChoice::Light => FontWeight::LIGHT,
            FontWeightChoice::Regular => FontWeight::NORMAL,
            FontWeightChoice::Medium => FontWeight::MEDIUM,
        };
        font
    }

    fn select(self, settings: &mut AppearanceSettings, family: &str) {
        match self {
            Self::Ui => settings.ui_font = family.into(),
            Self::Reading => settings.reading_font = family.into(),
            Self::Code => {
                settings.code_font = family.into();
                settings.theme.code_font = family.into();
            }
        }
    }
}

pub(super) fn code_font(cx: &App) -> Font {
    cx.try_global::<CodeTypography>().map_or_else(
        || gpui_kit::font(cx.theme().mono_font_family.clone()),
        |typography| typography.font.clone(),
    )
}

#[derive(PartialEq)]
struct UiTypography(Font);

impl gpui_kit::Global for UiTypography {}

struct WindowTypography;

impl Render for WindowTypography {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        Empty
    }
}

impl gpui_kit::base::RootPlugin for WindowTypography {
    fn style(&self, surface: &mut gpui_kit::Stateful<gpui_kit::Div>, _: &mut Window, cx: &mut App) {
        // Dialogs and native menu overlays are rendered alongside the workbench.
        if let Some(typography) = cx.try_global::<UiTypography>() {
            surface.style().refine(&StyleRefinement::default().font(typography.0.clone()));
        }
    }
}

pub(super) fn register_window_typography(cx: &mut App) {
    gpui_kit::base::Root::register_plugin(cx, |_, _| WindowTypography);
}

pub(super) struct Appearance {
    pub(super) settings: AppearanceSettings,
    path: Option<PathBuf>,
    error: Option<String>,
    override_theme: Option<ThemePreference>,
    accent_input: Entity<InputState>,
    accent_input_invalid: bool,
    pub(super) code_font: Font,
    pub(super) ui_font: Font,
    reading_font: Font,
    fonts: FontCatalog,
    font_selects: [Entity<FontSelect>; 3],
    pub(super) server_mode: ServerMode,
    pub(super) host_tailnet: bool,
    pub(super) keep_running: bool,
    server_fields: [Entity<InputState>; 7],
    window_save: Option<Task<()>>,
    pub pdf_style: Rc<Cell<PdfStyle>>,
}

#[cfg(test)]
fn settings_path() -> Option<PathBuf> {
    None
}

#[cfg(not(test))]
fn settings_path() -> Option<PathBuf> {
    #[cfg(all(target_os = "linux", feature = "headless-shot"))]
    if let Some(path) = fuzukue_proto::compat::env_os("FUZUKUE_SHOT_SETTINGS") {
        return Some(path.into());
    }
    std::env::current_exe()
        .ok()
        .map(|exe| exe.with_file_name("settings.toml"))
        .filter(|path| path.is_file())
        .or_else(|| {
            use fuzukue_proto::compat::{config_home, user_dir, LEGACY_APP_DIR};
            let base = config_home()?;
            let legacy = base.join(LEGACY_APP_DIR);
            // The old updater can still roll back while the trial marker exists.
            let dir = if crate::updater::legacy_config_trial() && legacy.is_dir() {
                legacy
            } else {
                user_dir(&base)
            };
            Some(dir.join("settings.toml"))
        })
}

fn load_settings(path: &Path) -> anyhow::Result<AppearanceSettings> {
    match fs::read_to_string(path) {
        Ok(text) => AppearanceSettings::from_toml(&text)
            .map_err(|_| anyhow::anyhow!(tr!("settings-invalid-toml-or-settings-value"))),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(AppearanceSettings::default()),
        Err(err) => Err(err.into()),
    }
}

pub(super) fn main_window_options(cx: &App) -> WindowOptions {
    let saved = settings_path()
        .and_then(|path| load_settings(&path).ok())
        .and_then(|settings| settings.main_window);
    let default = Bounds::centered(None, size(px(1400.), px(900.)), cx);
    let (window_bounds, display_id) = restore_window_placement(
        saved,
        cx.displays()
            .into_iter()
            .map(|display| (display.id(), display.bounds())),
        default,
    );
    WindowOptions {
        window_bounds: Some(window_bounds),
        display_id,
        ..gpui_kit::component::TitleBar::window_options()
    }
}

fn restore_window_placement(
    saved: Option<MainWindowPlacement>,
    displays: impl IntoIterator<Item = (DisplayId, Bounds<Pixels>)>,
    default: Bounds<Pixels>,
) -> (WindowBounds, Option<DisplayId>) {
    if let Some(saved) = saved {
        let bounds = Bounds::new(
            point(px(saved.x), px(saved.y)),
            size(px(saved.width), px(saved.height)),
        );
        // The Windows backend uses the center to validate placement on the
        // requested display and that display's scale to position the window.
        if let Some((id, _)) = displays
            .into_iter()
            .find(|(_, screen)| screen.contains(&bounds.center()))
        {
            let placement = if saved.maximized {
                WindowBounds::Maximized(bounds)
            } else {
                WindowBounds::Windowed(bounds)
            };
            return (placement, Some(id));
        }
    }
    (WindowBounds::Windowed(default), None)
}

#[cfg(windows)]
pub fn configure_wsl(arguments: Vec<String>) -> anyhow::Result<()> {
    let [distro, repo, project, ..] = &arguments[..] else {
        anyhow::bail!(
            tr!("settings-configure-wsl-usage")
        );
    };
    anyhow::ensure!(arguments.len() <= 4, tr!("settings-too-many-arguments-for-configure-wsl"));
    let path = settings_path().ok_or_else(|| anyhow::anyhow!(tr!("settings-settings-directory-not-found")))?;
    let mut settings = load_settings(&path)?;
    let server = &mut settings.server;
    let same_repo = server.distro == *distro && server.repo == *repo;
    if let Some(project) = arguments.get(3) {
        server.project.clone_from(project);
    } else if !same_repo || server.project.is_empty() {
        server.project.clone_from(project);
    }
    if server.mode == ServerMode::Local {
        server.addr = fuzukue_view::server::DEFAULT_ADDR.into();
    }
    server.mode = ServerMode::Wsl;
    server.distro.clone_from(distro);
    server.repo.clone_from(repo);
    server.launch(true).map_err(anyhow::Error::msg)?;
    crate::settings_file::initialize_model_setup(&path)?;
    crate::settings_file::save_appearance(&path, &settings)?;
    println!("{}", path.display());
    Ok(())
}

impl Appearance {
    pub fn new(window: &mut Window, cx: &mut Context<Workbench>) -> Self {
        let path = settings_path();
        let (settings, error) = match path.as_deref().map(load_settings) {
            Some(Ok(settings)) => (settings, None),
            Some(Err(err)) => (
                AppearanceSettings::default(),
                Some(tr!("settings-cannot-read-settings", err = format!("{:#}", err))),
            ),
            None => (
                AppearanceSettings::default(),
                Some(tr!("settings-settings-directory-not-found").into()),
            ),
        };
        if !cx.has_global::<SaveServerOnClose>() {
            cx.set_global(SaveServerOnClose(settings.save_server_on_close));
        }
        cx.observe_global::<SaveServerOnClose>(|_, cx| cx.notify()).detach();
        crate::i18n::apply(&settings.locale);
        let override_theme = fuzukue_proto::compat::env("FUZUKUE_THEME")
            .ok()
            .and_then(|value| ThemePreference::screenshot_override(&value));
        let accent_input = cx.new(|cx| {
            let value = match &settings.theme.accent {
                AccentChoice::Custom(value) => value.as_str(),
                AccentChoice::Preset(_) => "",
            };
            InputState::new(window, cx)
                .placeholder("#287b80")
                .default_value(value.to_owned())
        });
        cx.subscribe_in(&accent_input, window, |this, _, event: &InputEvent, window, cx| {
            match event {
                InputEvent::PressEnter { .. } => this.apply_custom_accent(window, cx),
                InputEvent::Change if this.appearance.accent_input_invalid => {
                    this.appearance.accent_input_invalid = false;
                    cx.notify();
                }
                _ => {}
            }
        })
        .detach();
        let fonts = FontCatalog::from_platform(cx);
        let font_selects = FONT_ROLES.map(|role| {
            let (options, selected) = fonts.options(role, role.saved(&settings));
            let state = cx.new(|cx| SelectState::new(options, selected, window, cx).searchable(true));
            cx.subscribe_in(
                &state,
                window,
                move |this, state, event: &SelectEvent<FontOptions>, window, cx| {
                    if let SelectEvent::Confirm(Some(family)) = event {
                        this.update_appearance(window, cx, |settings| role.select(settings, family));
                        let (options, _) = this.appearance.fonts.options(role, family);
                        state.update(cx, |state, cx| state.set_items(options, window, cx));
                    }
                },
            )
            .detach();
            state
        });
        let server = &settings.server;
        let server_mode = server.mode;
        let server_fields = [
            server.distro.as_str(),
            server.repo.as_str(),
            server.project.as_str(),
            server.addr.as_str(),
            server.binary.as_str(),
            server.url.as_str(),
            server.token.as_deref().unwrap_or_default(),
        ];
        let server_fields = std::array::from_fn(|index| {
            cx.new(|cx| {
                InputState::new(window, cx)
                    .masked(index == 6)
                    .default_value(server_fields[index].to_owned())
            })
        });
        let mut appearance = Self {
            code_font: FontRole::Code.resolve(&fonts, &settings),
            ui_font: FontRole::Ui.resolve(&fonts, &settings),
            reading_font: FontRole::Reading.resolve(&fonts, &settings),
            host_tailnet: settings.server.host_tailnet,
            keep_running: settings.server.keep_running,
            settings,
            path,
            error,
            override_theme,
            accent_input,
            accent_input_invalid: false,
            fonts,
            font_selects,
            server_mode,
            server_fields,
            window_save: None,
            pdf_style: Rc::new(Cell::new(PdfStyle::default())),
        };
        appearance.apply(window, cx);
        cx.observe_window_appearance(window, |this, window, cx| {
            if this.appearance.effective_theme() == ThemePreference::System {
                this.appearance.apply(window, cx);
                this.update_card_style_note_color(cx);
            }
        })
        .detach();
        if let Some(error) = &appearance.error {
            eprintln!("{error}");
        }
        appearance
    }

    pub fn send_queue_path(&self) -> Option<PathBuf> {
        Some(self.path.as_ref()?.with_extension("distribution-queue.json"))
    }

    pub(super) fn settings_path(&self) -> Option<PathBuf> {
        self.path.clone()
    }

    pub fn server_target(&self) -> Result<Target, &'static str> {
        Target::new(&self.settings.server, fuzukue_proto::compat::env("FUZUKUE_URL").ok())
    }

    #[cfg(all(target_os = "linux", feature = "headless-shot"))]
    pub(super) fn shot_theme(&mut self, theme: ThemePreference, window: &Window, cx: &mut App) {
        self.override_theme = Some(theme);
        self.apply(window, cx);
    }

    #[cfg(all(test, target_os = "linux", feature = "headless-shot"))]
    pub(super) fn isolate_shot_settings(&mut self, path: PathBuf) {
        self.path = Some(path);
        self.settings.thread_info_open = false;
    }

    fn effective_theme(&self) -> ThemePreference {
        self.override_theme.unwrap_or(self.settings.theme.base)
    }

    fn apply(&mut self, window: &Window, cx: &mut App) {
        let mode = match self.effective_theme() {
            ThemePreference::System => ThemeMode::from(window.appearance()),
            ThemePreference::Light => ThemeMode::Light,
            ThemePreference::Dark => ThemeMode::Dark,
        };
        if cx.theme().mode != mode {
            Theme::change(mode, None, cx);
        }
        let resolve = |role: FontRole| role.resolve(&self.fonts, &self.settings);
        self.ui_font = resolve(FontRole::Ui);
        self.reading_font = resolve(FontRole::Reading);
        self.code_font = resolve(FontRole::Code);
        let ui_font = &self.ui_font.family;
        let code_font = &self.code_font.family;
        let ui_size = px(self.settings.ui_font_size);
        let theme = cx.theme();
        let surfaces = ThemeSurfaces::for_mode(mode);
        let accent = accent_color(&self.settings.theme.accent, theme.secondary, mode.is_dark());
        let menu_highlight = theme.muted.blend(accent.opacity(0.14));
        if theme.font_family != *ui_font
            || theme.mono_font_family != *code_font
            || theme.font_size != ui_size
            || theme.scrollbar_mode != ScrollbarMode::Always
            || theme.background != surfaces.background
            || theme.sidebar != surfaces.chrome
            || theme.title_bar != surfaces.chrome
            || theme.sidebar_border != surfaces.divider
            || theme.title_bar_border != surfaces.divider
            || theme.border != surfaces.outline
            || theme.input != surfaces.outline
            || theme.link != accent
            || theme.ring != accent
            || theme.accent != menu_highlight
        {
            Theme::update(cx, |theme| {
                theme.font_family = ui_font.clone();
                theme.font_size = ui_size;
                theme.mono_font_family = code_font.clone();
                theme.scrollbar_mode = ScrollbarMode::Always;
                theme.background = surfaces.background;
                theme.sidebar = surfaces.chrome;
                theme.title_bar = surfaces.chrome;
                // Use the chrome divider for structural rules and map both of
                // Kit's control border fields to the same outline.
                theme.sidebar_border = surfaces.divider;
                theme.title_bar_border = surfaces.divider;
                theme.border = surfaces.outline;
                theme.input = surfaces.outline;
                theme.link = accent;
                theme.link_hover = accent;
                theme.link_active = accent;
                theme.ring = accent;
                theme.accent = menu_highlight;
            });
        }
        let code_typography = CodeTypography {
            font: self.code_font.clone(),
            size_scale: self.settings.code_font_scale(),
        };
        if cx.try_global::<CodeTypography>() != Some(&code_typography) {
            cx.set_global(code_typography);
        }
        if cx.try_global::<UiTypography>().map(|typography| &typography.0) != Some(&self.ui_font) {
            cx.set_global(UiTypography(self.ui_font.clone()));
        }
        cx.set_global(math::MathScale(self.settings.math_scale));
        let theme = cx.theme();
        self.pdf_style.set(PdfStyle {
            bg: theme.muted,
            border: theme.border,
            placeholder_bg: theme.secondary,
            placeholder_fg: theme.muted_foreground,
            header_fg: theme.foreground,
            header_muted: theme.muted_foreground,
        });
    }

    fn save(&self) -> anyhow::Result<()> {
        let path = self
            .path
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!(tr!("settings-settings-directory-not-found")))?;
        crate::settings_file::save_appearance(path, &self.settings)
    }

    pub(super) fn track_main_window(window: &mut Window, cx: &Context<Workbench>) {
        cx.observe_window_bounds(window, |this, window, cx| {
            this.appearance.window_save = Some(cx.spawn_in(window, async move |this, cx| {
                cx.background_executor().timer(Duration::from_millis(250)).await;
                let _ = this.update_in(cx, |this, window, _| this.appearance.save_window(window));
            }));
        })
        .detach();

        let handle = window.window_handle();
        cx.on_app_quit(move |this, cx| {
            let _ = handle.update(cx, |_, window, _| this.appearance.save_window(window));
            std::future::ready(())
        })
        .detach();
    }

    pub(super) fn save_window(&mut self, window: &Window) {
        let (bounds, maximized) = match window.window_bounds() {
            WindowBounds::Windowed(bounds) => (bounds, false),
            WindowBounds::Maximized(bounds) => (bounds, true),
            WindowBounds::Fullscreen(_) => return,
        };
        let placement = Some(MainWindowPlacement {
            x: bounds.origin.x.into(),
            y: bounds.origin.y.into(),
            width: bounds.size.width.into(),
            height: bounds.size.height.into(),
            maximized,
        });
        if placement != self.settings.main_window {
            let previous = std::mem::replace(&mut self.settings.main_window, placement);
            if let Err(error) = self.save() {
                self.settings.main_window = previous;
                eprintln!("{}", tr!("settings-cannot-save-window-position", detail = format!("{error:#}")));
            }
        }
    }

    pub(super) fn paper_chat_width(&self) -> f32 {
        self.settings.paper_chat_width
    }

    pub(super) fn thread_info_open(&self) -> bool {
        self.settings.thread_info_open
    }

    pub(super) fn send_comments_here(&self) -> bool {
        !self.settings.comments_to_side_thread
    }

    pub(super) fn toggle_thread_info(&mut self) -> anyhow::Result<()> {
        self.settings.thread_info_open = !self.settings.thread_info_open;
        self.save()
    }

    pub(super) fn remember_comment_destination(&mut self, here: bool) -> anyhow::Result<()> {
        self.settings.comments_to_side_thread = !here;
        self.save()
    }

    pub(super) fn sidebar_width(&self) -> f32 {
        self.settings.sidebar_width
    }

    pub(super) fn remember_sidebar_width(&mut self, width: f32) -> anyhow::Result<()> {
        if self.settings.sidebar_width == width {
            return Ok(());
        }
        self.settings.sidebar_width = width;
        self.settings.normalize();
        self.save()
    }

    pub fn style_reading<E: Styled>(&self, element: E) -> E {
        element
            .font(self.reading_font.clone())
            .text_size(px(self.settings.reading_font_size))
            .line_height(relative(self.settings.reading_line_height))
    }

    pub(super) fn style_paper<E: Styled>(&self, element: E) -> E {
        // The workbench already supplies the UI font used by the chat.
        element
            .text_size(px(self.settings.paper_font_size))
            .line_height(relative(self.settings.paper_line_height))
    }
}

struct NumberSetting {
    id: &'static str,
    label: &'static str,
    value: fn(&mut AppearanceSettings) -> &mut f32,
    range: std::ops::RangeInclusive<f32>,
    step: f32,
    unit: &'static str,
}

const NUMBERS: [NumberSetting; 7] = [
    NumberSetting {
        id: "code-size",
        label: "settings-code-text-size",
        value: |s| &mut s.code_font_size_percent,
        range: CODE_FONT_SIZE_PERCENT,
        step: 1.0,
        unit: "%",
    },
    NumberSetting {
        id: "ui-size",
        label: "settings-ui-text-size",
        value: |s| &mut s.ui_font_size,
        range: UI_FONT_SIZE,
        step: 1.0,
        unit: " px",
    },
    NumberSetting {
        id: "reading-size",
        label: "settings-reading-text-size",
        value: |s| &mut s.reading_font_size,
        range: READING_FONT_SIZE,
        step: 1.0,
        unit: " px",
    },
    NumberSetting {
        id: "reading-leading",
        label: "settings-reading-line-height",
        value: |s| &mut s.reading_line_height,
        range: READING_LINE_HEIGHT,
        step: 0.1,
        unit: "settings-scale-unit",
    },
    NumberSetting {
        id: "paper-size",
        label: "settings-paper-text-size",
        value: |s| &mut s.paper_font_size,
        range: READING_FONT_SIZE,
        step: 1.0,
        unit: " px",
    },
    NumberSetting {
        id: "paper-leading",
        label: "settings-paper-line-height",
        value: |s| &mut s.paper_line_height,
        range: READING_LINE_HEIGHT,
        step: 0.1,
        unit: "settings-scale-unit",
    },
    NumberSetting {
        id: "math-scale",
        label: "settings-math-scale",
        value: |s| &mut s.math_scale,
        range: MATH_SCALE,
        step: 0.05,
        unit: "settings-scale-unit",
    },
];

impl Workbench {
    fn apply_custom_accent(&mut self, window: &Window, cx: &mut Context<Self>) {
        let value = self.appearance.accent_input.read(cx).value();
        let Some(hex) = parse_hex_color(&value) else {
            self.appearance.accent_input_invalid = true;
            cx.notify();
            return;
        };
        self.appearance.accent_input_invalid = false;
        self.update_appearance(window, cx, |settings| {
            settings.theme.accent = AccentChoice::Custom(format!("#{hex:06x}"));
        });
        cx.notify();
    }

    fn render_accent_settings(&self, cx: &mut Context<Self>) -> gpui_kit::Div {
        let accent = &self.appearance.settings.theme.accent;
        let theme = cx.theme();
        let dark = theme.is_dark();
        let (applied, error_color, note_color) = (theme.link, theme.danger, theme.muted_foreground);
        let adjusted = matches!(
            accent,
            AccentChoice::Custom(value)
                if parse_hex_color(value).is_some_and(|hex| Hsla::from(rgb(hex)) != applied)
        );
        let mut presets = h_flex().gap_2().flex_wrap();
        for swatch in ACCENT_SWATCHES {
            let preset = swatch.preset;
            presets = presets.child(
                Button::new(swatch.label)
                    .small()
                    .label(tr!(swatch.label))
                    .toggled(*accent == AccentChoice::Preset(preset))
                    .child(div().size(px(10.)).rounded_full().bg(swatch.color(dark)))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.appearance.accent_input_invalid = false;
                        this.appearance.accent_input.update(cx, |input, cx| {
                            input.set_value("", window, cx);
                        });
                        this.update_appearance(window, cx, |settings| {
                            settings.theme.accent = AccentChoice::Preset(preset);
                        });
                        cx.notify();
                    })),
            );
        }
        let custom_label = tr!("settings-accent-custom");
        v_flex()
            .gap_1()
            .child(tr!("settings-accent"))
            .child(presets)
            .child(div().text_sm().child(custom_label))
            .child(
                h_flex().gap_2()
                    .child(Input::new(&self.appearance.accent_input).w(px(120.)).aria_label(custom_label))
                    .child(Button::new("apply-custom-accent")
                        .label(tr!("settings-accent-apply"))
                        .on_click(cx.listener(|this, _, window, cx| this.apply_custom_accent(window, cx))))
                    .when(matches!(accent, AccentChoice::Custom(_)), |row| {
                        row.child(div().size(px(10.)).rounded_full().bg(applied))
                    }),
            )
            .when(self.appearance.accent_input_invalid, |form| {
                form.child(div().text_sm().text_color(error_color).child(tr!("settings-accent-invalid")))
            })
            .when(adjusted && !self.appearance.accent_input_invalid, |form| {
                form.child(div().text_sm().text_color(note_color).child(tr!("settings-accent-adjusted")))
            })
    }

    fn update_appearance(
        &mut self,
        window: &Window,
        cx: &mut Context<Self>,
        edit: impl FnOnce(&mut AppearanceSettings),
    ) {
        let previous = self.appearance.settings.clone();
        edit(&mut self.appearance.settings);
        self.appearance.settings.normalize();
        let current = &self.appearance.settings;
        if previous == *current && self.appearance.error.is_none() {
            return;
        }
        if previous.show_worker_messages != current.show_worker_messages {
            for chat in self.chats.values_mut() {
                chat.reset_worker_disclosures();
            }
            self.workshop.report_open = None;
            self.handoffs.reset_worker_disclosures();
        }
        if previous.ui_font != current.ui_font
            || previous.ui_font_weight != current.ui_font_weight
            || previous.ui_font_size != current.ui_font_size
            || previous.math_scale != current.math_scale
            || previous.theme.code_font != current.theme.code_font
            || previous.code_font != current.code_font
            || previous.code_font_weight != current.code_font_weight
            || previous.code_font_size_percent != current.code_font_size_percent
        {
            for chat in self.chats.values() {
                chat.scroller.remeasure();
            }
        }
        self.read_list.remeasure();
        self.appearance.apply(window, cx);
        self.update_card_style_note_color(cx);
        self.appearance.error = self
            .appearance
            .save()
            .err()
            .map(|e| tr!("settings-cannot-save-settings", e = e.to_string()));
        cx.notify();
    }

    #[cfg(all(target_os = "linux", feature = "headless-shot"))]
    pub(super) fn shot_locale(&mut self, tag: String, window: &mut Window, cx: &mut Context<Self>) {
        self.apply_locale(&tag, window, cx);
        self.appearance.settings.locale = tag;
        cx.notify();
    }

    fn apply_locale(&mut self, preference: &str, window: &mut Window, cx: &mut Context<Self>) {
        let previous = crate::i18n::current();
        crate::i18n::apply(preference);
        let locale = crate::i18n::current();
        if previous == locale {
            return;
        }
        if self.protocol >= fuzukue_proto::LOCALE_PROTOCOL {
            let _ = self.out.send(fuzukue_proto::ClientMsg::SetLocale { locale: locale.tag().into() });
        }
        self.refresh_extension_locale();
        self.input.update(cx, |input, cx| input.set_placeholder(tr!("workbench-enter-a-message"), window, cx));
        self.comment_input.update(cx, |input, cx| input.set_placeholder(tr!("workbench-write-a-comment"), window, cx));
        for chat in self.chats.values() {
            chat.scroller.remeasure();
        }
        cx.notify();
    }

    pub(super) fn change_text_size(
        &mut self,
        delta: f32,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        let paper = self.right == super::RightPane::Read && !self.settings_open;
        self.update_appearance(window, cx, |s| {
            if paper {
                s.paper_font_size += delta;
            } else {
                s.reading_font_size += delta;
            }
        });
    }

    pub(super) fn set_auto_update(&mut self, auto: bool, cx: &mut Context<Self>) {
        let previous = std::mem::replace(&mut self.appearance.settings.server.auto_update, auto);
        match self.appearance.save() {
            Ok(()) => {
                self.appearance.error = None;
                if auto {
                    self.replace_client(cx);
                }
            }
            Err(err) => {
                self.appearance.settings.server.auto_update = previous;
                self.appearance.error = Some(tr!("settings-cannot-save-settings-appearance", err = format!("{err:#}")));
            }
        }
        cx.notify();
    }

    fn apply_server_settings(&mut self, cx: &mut Context<Self>) {
        let [distro, repo, project, addr, binary, url, token] = self
            .appearance
            .server_fields
            .each_ref()
            .map(|field| field.read(cx).value().to_string());
        let settings = ServerSettings {
            mode: self.appearance.server_mode,
            distro,
            repo,
            project,
            addr,
            binary,
            url: url.trim().to_owned(),
            token: (!token.trim().is_empty()).then(|| token.trim().to_owned()),
            host_tailnet: matches!(self.appearance.server_mode, ServerMode::Local | ServerMode::Wsl)
                && self.appearance.host_tailnet,
            keep_running: self.appearance.keep_running,
            auto_update: self.appearance.settings.server.auto_update,
        };
        let target = match Target::new(&settings, fuzukue_proto::compat::env("FUZUKUE_URL").ok()) {
            Ok(target) => target,
            Err(err) => {
                self.appearance.error = Some(err.into());
                cx.notify();
                return;
            }
        };
        let previous = std::mem::replace(&mut self.appearance.settings.server, settings);
        match self.appearance.save() {
            Ok(()) => {
                self.appearance.error = None;
                self.status = super::ConnectionStatus::Switching;
                self.distribution.reset_connection();
                self.connection.restart(target);
            }
            Err(err) => {
                self.appearance.settings.server = previous;
                self.appearance.error = Some(tr!("settings-cannot-save-settings-appearance", err = format!("{:#}", err)));
            }
        }
        cx.notify();
    }

    fn render_server_settings(&self, cx: &mut Context<Self>) -> gpui_kit::Div {
        let theme = cx.theme().clone();
        let mode = self.appearance.server_mode;
        let platform_mode = if cfg!(windows) {
            (ServerMode::Wsl, "server-wsl", "WSL")
        } else {
            (ServerMode::Local, "server-local", tr!("settings-local"))
        };
        let mut modes = h_flex().gap_2().flex_wrap();
        for (choice, id, label) in [
            (ServerMode::External, "server-external", tr!("settings-external")),
            (ServerMode::Remote, "server-remote", tr!("settings-remote")),
            platform_mode,
        ] {
            modes = modes.child(
                Button::new(id)
                    .label(label)
                    .toggled(choice == mode)
                    .when(choice == mode, |button| button.primary())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.appearance.server_mode = choice;
                        cx.notify();
                    })),
            );
        }
        let mut form = v_flex()
            .gap_2()
            .border_t_1()
            .border_color(theme.sidebar_border)
            .pt_4()
            .child(div().text_lg().child(tr!("settings-server")))
            .child(modes);
        let labels: &[(usize, &str)] = match mode {
            ServerMode::Wsl => &[
                (0, tr!("settings-distribution-blank-for-default")),
                (1, tr!("settings-repository-absolute-wsl-path")),
                (2, tr!("settings-project-absolute-wsl-path")),
                (3, tr!("settings-listen-address")),
            ],
            ServerMode::Local => &[
                (2, tr!("settings-project-absolute-path")),
                (3, tr!("settings-listen-address")),
                (4, tr!("settings-server-executable-blank-to-search-path")),
            ],
            ServerMode::Remote => &[
                (5, tr!("settings-server-url-ws-host-port-ws")),
                (6, tr!("settings-connection-token-optional")),
            ],
            ServerMode::External => &[],
        };
        for &(index, label) in labels {
            let state = &self.appearance.server_fields[index];
            form = form.child(v_flex().gap_1().child(label).child(Input::new(state)));
        }
        let label = if matches!(mode, ServerMode::External | ServerMode::Remote) {
            tr!("settings-save-and-reconnect")
        } else {
            tr!("settings-restart-server")
        };
        form.children(self.render_self_update(cx))
            .child(self.render_distribution_settings(cx))
            .child(
            Button::new("restart-server")
                .label(label)
                .on_click(cx.listener(|this, _, _, cx| this.apply_server_settings(cx))),
        )
        .child(
            v_flex()
                .text_sm()
                .text_color(theme.muted_foreground)
                .children(self.status.text().lines().map(|line| div().child(line.to_owned()))),
        )
    }

    pub(super) fn screenshot_without_prompt(&self) -> bool {
        self.appearance.settings.screenshot_without_prompt
    }

    pub(super) fn save_server_on_close(&self, cx: &App) -> bool {
        cx.global::<SaveServerOnClose>().0
    }

    fn set_save_server_on_close(&mut self, enabled: bool, cx: &mut Context<Self>) {
        let result = self.appearance.settings_path().ok_or_else(||
            anyhow::anyhow!(tr!("settings-settings-directory-not-found")))
            .and_then(|path| crate::settings_file::save_server_on_close(&path, enabled));
        if let Err(error) = result {
            self.appearance.error = Some(tr!("workbench-cannot-update-settings", detail = format!("{error:#}")));
        } else {
            cx.global_mut::<SaveServerOnClose>().0 = enabled;
            self.appearance.error = None;
        }
        cx.notify();
    }

    pub(super) fn render_settings_pane(&self, cx: &mut Context<Self>) -> gpui_kit::Div {
        use crate::agent_management::Tab;
        let managed = self.protocol >= fuzukue_proto::AGENT_MANAGEMENT_PROTOCOL && !self.sidebar.server_settings;
        let show_notifications = self.notifications.settings_selected && !self.sidebar.server_settings;
        let theme = cx.theme().clone();
        let mut themes = h_flex().gap_2().flex_wrap();
        for (preference, id, key) in THEMES {
            themes = themes.child(
                Button::new(id)
                    .line_height(UI_LINE_HEIGHT)
                    .label(tr!(key))
                    .toggled(self.appearance.effective_theme() == preference)
                    .when(self.appearance.effective_theme() == preference, |button| {
                        button.primary()
                    })
                    .disabled(self.appearance.override_theme.is_some())
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.update_appearance(window, cx, |s| s.theme.base = preference);
                    })),
            );
        }

        let mut languages = h_flex().gap_2().flex_wrap();
        let other_languages = fuzukue_i18n::Locale::available()
            .filter(|locale| *locale != fuzukue_i18n::Locale::JAPANESE)
            .map(fuzukue_i18n::Locale::tag);
        for tag in ["system", "ja"].into_iter().chain(other_languages) {
            let key = format!("settings-language-{tag}");
            languages = languages.child(
                Button::new(SharedString::from(format!("locale-{tag}")))
                    .line_height(UI_LINE_HEIGHT)
                    .label(tr!(&key))
                    .toggled(self.appearance.settings.locale == tag)
                    .when(self.appearance.settings.locale == tag, |button| button.primary())
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.apply_locale(tag, window, cx);
                        this.update_appearance(window, cx, |s| s.locale = tag.into());
                    })),
            );
        }

        let mut form = v_flex()
            .gap_4()
            .child(v_flex().gap_1().child(tr!("settings-language")).child(languages))
            .child(div().text_lg().child(tr!("settings-appearance")))
            .child({
                let view = cx.weak_entity();
                gpui_kit::component::switch::Switch::new("save-server-on-close")
                    .label(tr!("settings-save-server-on-close"))
                    .checked(self.save_server_on_close(cx))
                    .on_change(move |checked, _, cx| {
                        let _ = view.update(cx, |this, cx| {
                            this.set_save_server_on_close(*checked, cx);
                        });
                    })
            })
            .child(v_flex().gap_1().child(tr!("settings-theme")).child(themes))
            .child(self.render_accent_settings(cx))
            .child({
                let view = cx.weak_entity();
                gpui_kit::component::switch::Switch::new("show-worker-messages")
                    .label(tr!("settings-show-worker-messages"))
                    .checked(self.appearance.settings.show_worker_messages)
                    .on_change(move |checked, window, cx| {
                        let _ = view.update(cx, |this, cx| {
                            this.update_appearance(window, cx, |s| s.show_worker_messages = *checked);
                        });
                    })
            })
            .children((self.protocol >= fuzukue_proto::EXTENSIONS_PROTOCOL && !self.sidebar.server_settings)
                .then(|| self.render_extension_settings(cx)))
            .child(v_flex().gap_1().child(tr!("screenshot-agent-settings")).child({
                let view = cx.weak_entity();
                gpui_kit::component::switch::Switch::new("screenshot-without-prompt")
                    .label(tr!("screenshot-without-prompt"))
                    .checked(self.screenshot_without_prompt())
                    .on_change(move |checked, window, cx| {
                        let _ = view.update(cx, |this, cx| {
                            this.update_appearance(window, cx, |s| s.screenshot_without_prompt = *checked);
                        });
                    })
            }))
            .child(self.role_defaults(cx))
            .children((self.protocol >= 14 && !managed).then(|| self.render_agent_settings(cx)));
        if let Some(mode) = self.appearance.override_theme {
            let value = if mode == ThemePreference::Dark {
                "dark"
            } else {
                "light"
            };
            form = form.child(
                div()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(tr!("settings-locked-by-fuzukue-theme", value = value.to_string())),
            );
        }
        for (((id, label, weight_label, font), role), state) in [
            (
                "ui",
                tr!("settings-ui-font"),
                tr!("settings-ui-font-weight"),
                &self.appearance.ui_font,
            ),
            (
                "reading",
                tr!("settings-reading-font"),
                tr!("settings-reading-font-weight"),
                &self.appearance.reading_font,
            ),
            (
                "code",
                tr!("settings-code-font"),
                tr!("settings-code-font-weight"),
                &self.appearance.code_font,
            ),
        ]
        .into_iter()
        .zip(FONT_ROLES)
        .zip(&self.appearance.font_selects)
        {
            form = form.child(
                v_flex()
                    .gap_1()
                    .child(label)
                    .child(
                        Select::new(state)
                            .id(SharedString::from(format!("{id}-font")))
                            .w_full()
                            .accessibility_label(label)
                            .search_placeholder(tr!("settings-search-fonts")),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .text_sm()
                            .child(
                                div()
                                    .text_color(theme.muted_foreground)
                                    .child(tr!("settings-font-preview")),
                            )
                            .child(div().font(font.clone()).child(tr!("settings-font-sample"))),
                    )
                    .child(div().text_sm().child(weight_label))
                    .child(h_flex().gap_2().flex_wrap().children(
                        [
                            (FontWeightChoice::Light, "light", tr!("settings-font-weight-light")),
                            (FontWeightChoice::Regular, "regular", tr!("settings-font-weight-regular")),
                            (FontWeightChoice::Medium, "medium", tr!("settings-font-weight-medium")),
                        ]
                        .map(|(weight, suffix, label)| {
                            let selected = role.weight(&self.appearance.settings) == weight;
                            Button::new(SharedString::from(format!("{id}-weight-{suffix}")))
                                .line_height(UI_LINE_HEIGHT)
                                .label(label)
                                .accessibility_label(format!("{weight_label} · {label}"))
                                .tooltip(tr!("settings-font-weight-fallback"))
                                .toggled(selected)
                                .when(selected, |button| button.primary())
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.update_appearance(window, cx, |settings| {
                                        role.select_weight(settings, weight);
                                    });
                                }))
                        }),
                    )),
            );
        }
        let mut settings = self.appearance.settings.clone();
        for spec in NUMBERS {
            let value = *(spec.value)(&mut settings);
            let unit = match spec.unit {
                " px" | "%" => spec.unit,
                unit => tr!(unit),
            };
            let stepper = |suffix, symbol: Symbol, action, delta: f32, disabled| {
                let field = spec.value;
                Button::new(SharedString::from(format!("{}-{suffix}", spec.id)))
                    .icon(symbol.icon())
                    .accessibility_label(tr!("settings-adjust-value", setting = tr!(spec.label), action = action))
                    .disabled(disabled)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.update_appearance(window, cx, |s| {
                            let value = field(s);
                            *value = ((*value + delta) * 100.0).round() / 100.0;
                        });
                    }))
            };
            form = form.child(
                v_flex().gap_1().child(tr!(spec.label)).child(
                    h_flex()
                        .gap_2()
                        .child(stepper(
                            "down",
                            Symbol::Minus,
                            tr!("settings-decrease"),
                            -spec.step,
                            value <= *spec.range.start(),
                        ))
                        .child(
                            div()
                                .min_w(px(72.))
                                .text_center()
                                .child(format!("{value}{unit}")),
                        )
                        .child(stepper(
                            "up",
                            Symbol::Plus,
                            tr!("settings-increase"),
                            spec.step,
                            value >= *spec.range.end(),
                        )),
                ),
            );
        }
        form = if self.sidebar.server_settings {
            self.render_server_settings(cx)
        } else {
            form.child(self.render_github_settings(cx))
                .child(self.render_server_settings(cx))
        };
        if let Some(error) = &self.appearance.error {
            form = form.child(
                div()
                    .text_sm()
                    .text_color(theme.danger)
                    .child(error.clone()),
            );
        }

        if show_notifications {
            form = self.render_notification_settings(cx);
        } else if managed {
            form = match self.agent_management.tab {
                Tab::General => form,
                Tab::Agents => v_flex().child(self.render_agent_settings(cx)),
                Tab::History => self.render_agent_history(cx),
            };
        }
        v_flex()
            .size_full()
            .min_w_0()
            .child(
                h_flex()
                    .id("settings-pane-header")
                    .px(PANE_INSET)
                    .py_2()
                    .gap_2()
                    .justify_between()
                    .border_b_1()
                    .border_color(theme.sidebar_border)
                    .child(div().id("settings-pane-title").min_w_0().text_lg().child(if self.sidebar.server_settings { tr!("settings-connection") } else { tr!("settings-title") }))
                    .child(h_flex().gap_2()
                        .children((!self.sidebar.server_settings).then(||
                            Button::new("settings-notifications").ghost().small()
                                .label(tr!("notifications-heading"))
                                .toggled(show_notifications)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.notifications.settings_selected = !this.notifications.settings_selected;
                                    cx.notify();
                                }))))
                        .child(self.pane_close_button("close-settings", cx))),
            )
            .children((managed && !show_notifications).then(|| h_flex().gap_2().px(PANE_INSET).py_2().flex_wrap().border_b_1().border_color(theme.sidebar_border)
                .children([(Tab::General, "settings-general", tr!("settings-general")), (Tab::Agents, "settings-agents", tr!("settings-agents")), (Tab::History, "settings-agent-history", tr!("settings-history"))]
                    .map(|(tab, id, label)| Button::new(id).line_height(UI_LINE_HEIGHT).label(label).toggled(self.agent_management.tab == tab)
                        .on_click(cx.listener(move |this, _, window, cx| this.open_agent_management(tab, window, cx)))))))
            .child(
                div()
                    .id(if show_notifications { "notifications-scroll" } else { match (managed, self.agent_management.tab) {
                        (true, Tab::Agents) => "agent-settings-scroll",
                        (true, Tab::History) => "agent-history-scroll",
                        _ => "appearance-settings",
                    } })
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scrollbar()
                    .tab_group()
                    .child(form.px(PANE_INSET).py_4()),
            )
    }
}

#[cfg(test)]
mod accent_tests {
    use super::*;

    #[test]
    fn style_note_mark_is_muted_and_has_contrast_in_both_themes() {
        gpui_kit::platform::headless().run(|cx| {
            gpui_kit::init(cx);
            for mode in [ThemeMode::Light, ThemeMode::Dark] {
                Theme::change(mode, None, cx);
                let theme = cx.theme();
                let background = ThemeSurfaces::for_mode(mode).background;
                let mark = style_note_warning(theme.warning, background);
                assert!((3. ..= 3.25).contains(&contrast_ratio(mark, background)), "{mode:?}");
                let style = &theme.highlight_theme.style;
                let editor = editor_style_note_warning(theme);
                for surface in [style.editor_background, style.editor_active_line, style.editor_gutter_background].into_iter().flatten() {
                    assert!(contrast_ratio(editor, surface) >= 3., "{mode:?}");
                }
            }
            cx.spawn(async move |cx| { let _ = cx.update(|cx| cx.quit()); }).detach();
        });
    }

    #[test]
    fn contrast_ratio_matches_srgb_reference_colors() {
        let black = rgb(0).into();
        let white = rgb(0xffffff).into();
        let gray = rgb(0x777777).into();
        assert_eq!(contrast_ratio(black, black), 1.0);
        assert!((contrast_ratio(black, white) - 21.0).abs() < 0.001);
        assert!((contrast_ratio(white, gray) - 4.478).abs() < 0.001);
        assert_eq!(contrast_ratio(white, gray), contrast_ratio(gray, white));
    }

    #[test]
    fn theme_surfaces_keep_chrome_subtle_and_chat_distinct() {
        for mode in [ThemeMode::Light, ThemeMode::Dark] {
            let surfaces = ThemeSurfaces::for_mode(mode);
            let chrome_ratio = contrast_ratio(surfaces.background, surfaces.chrome);
            let chat_ratio = contrast_ratio(surfaces.background, surfaces.chat);
            let divider_ratio = contrast_ratio(surfaces.chrome, surfaces.divider);
            assert!((1.05..=1.09).contains(&chrome_ratio), "{mode:?}: {chrome_ratio}");
            assert!((1.12..=1.18).contains(&chat_ratio), "{mode:?}: {chat_ratio}");
            assert!(divider_ratio >= 1.5, "{mode:?}: {divider_ratio}");
        }
    }

    #[test]
    fn code_surface_has_contrast_on_chat_page_and_user_bubble() {
        use gpui_kit::base::ColorTokens;

        for (mode, colors) in [
            (ThemeMode::Light, ColorTokens::light()),
            (ThemeMode::Dark, ColorTokens::dark()),
        ] {
            let surfaces = ThemeSurfaces::for_mode(mode);
            for (container, parent) in [
                ("assistant page", surfaces.background),
                ("user bubble", surfaces.chat),
            ] {
                let ratio = contrast_ratio(surfaces.code, parent);
                assert!(luminance(surfaces.code) < luminance(parent), "{mode:?} {container}");
                assert!(ratio >= 1.12, "{mode:?} {container} {ratio}");
            }
            let text_ratio = contrast_ratio(colors.foreground, surfaces.code);
            assert!(text_ratio >= 4.5, "{mode:?} code text {text_ratio}");
        }
    }

    #[test]
    fn hex_input_accepts_only_three_or_six_hex_digits() {
        assert_eq!(parse_hex_color("#aB3"), Some(0xaabb33));
        assert_eq!(parse_hex_color(" #012aBc "), Some(0x012abc));
        for invalid in ["", "123", "#12", "#1234", "#12345678", "#+12", "#zzzzzz", "#é0000"] {
            assert_eq!(parse_hex_color(invalid), None, "{invalid}");
        }
    }

    #[test]
    fn custom_accent_adjusts_lightness_only_when_needed() {
        for (mode, surface, raw, expected) in [
            (ThemeMode::Dark, 0x262626, 0x113344, 0x309ea4),
            (ThemeMode::Light, 0xe5e5e5, 0xaaccdd, 0x246c72),
        ] {
            let dark = mode.is_dark();
            let surface = rgb(surface).into();
            let raw: Hsla = rgb(raw).into();
            let foreground = button_foreground(dark);
            assert!(contrast_ratio(raw, foreground) < 4.5);
            let adjusted = accessible_accent(raw, surface, dark, 4.5);
            assert_eq!(adjusted.h, raw.h);
            assert_eq!(adjusted.s, raw.s);
            assert_eq!(adjusted.l > raw.l, dark);
            assert!(contrast_ratio(adjusted, foreground) >= 4.5);
            assert!(contrast_ratio(adjusted, surface) >= 4.5);
            assert!(contrast_ratio(adjusted, ThemeSurfaces::for_mode(mode).chat) >= 4.5);
            for color in accent_button_states(adjusted, surface, dark) {
                assert!(contrast_ratio(color, foreground) >= 4.5);
                assert!(contrast_ratio(color, surface) >= 3.0);
            }
            let expected = rgb(expected).into();
            assert_eq!(accessible_accent(expected, surface, dark, 4.5), expected);
        }
    }

    #[test]
    fn composer_neutral_icons_meet_contrast_in_both_modes() {
        for mode in [ThemeMode::Light, ThemeMode::Dark] {
            let surfaces = ThemeSurfaces::for_mode(mode);
            let (disabled, arrow) = surfaces.composer_disabled(mode.is_dark());
            let (stop, square) = surfaces.composer_stop(mode.is_dark());
            for (fill, icon) in [(disabled, arrow), (stop, square), (surfaces.chat, stop)] {
                assert!(contrast_ratio(fill, icon) >= 3.);
            }
            assert!(contrast_ratio(stop, disabled) >= 3.);
        }
    }

    #[test]
    fn every_preset_and_send_state_meet_contrast_in_both_modes() {
        for (mode, secondary) in [(ThemeMode::Light, 0xe5e5e5), (ThemeMode::Dark, 0x262626)] {
            let dark = mode.is_dark();
            let secondary = rgb(secondary).into();
            let surfaces = ThemeSurfaces::for_mode(mode);
            let foreground = button_foreground(dark);
            for swatch in ACCENT_SWATCHES {
                let normal = swatch.color(dark);
                assert_eq!(accent_color(&AccentChoice::Preset(swatch.preset), secondary, dark), normal);
                assert!(contrast_ratio(normal, surfaces.chat) >= 4.5, "{}", swatch.label);
                let [hover, pressed, disabled] = accent_button_states(normal, secondary, dark);
                assert!(hover.l > normal.l);
                assert!(pressed.l < normal.l);
                assert!(disabled.s < normal.s);
                for color in [normal, hover, pressed, disabled] {
                    assert!(contrast_ratio(color, foreground) >= 4.5, "{}", swatch.label);
                    assert!(contrast_ratio(color, surfaces.background) >= 3.0, "{}", swatch.label);
                    assert!(contrast_ratio(color, surfaces.chat) >= 3.0, "{}", swatch.label);
                }
            }
        }
    }
}

#[cfg(test)]
mod window_tests {
    use super::*;

    #[test]
    fn ui_line_height_scales_with_the_label_font_size() {
        for rem in [*UI_FONT_SIZE.start(), 16., *UI_FONT_SIZE.end()] {
            for text_scale in [0.75, 0.875, 1.] {
                let label_size = px(rem * text_scale);
                let line = UI_LINE_HEIGHT.to_pixels(label_size.into(), px(rem));
                assert!(line >= label_size * 1.5);
            }
        }
    }

    #[test]
    fn restores_on_the_saved_display_or_uses_the_centred_default() {
        let primary = DisplayId::new(1);
        let secondary = DisplayId::new(2);
        let screen = Bounds::new(point(px(0.), px(0.)), size(px(1920.), px(1080.)));
        let second_screen = Bounds::new(point(px(-1600.), px(0.)), size(px(1600.), px(900.)));
        let default = Bounds::centered_at(screen.center(), size(px(1400.), px(900.)));
        let saved = MainWindowPlacement {
            x: -1500.,
            y: 100.,
            width: 1200.,
            height: 720.,
            maximized: true,
        };
        let bounds = Bounds::new(
            point(px(saved.x), px(saved.y)),
            size(px(saved.width), px(saved.height)),
        );
        let screens = [(primary, screen), (secondary, second_screen)];
        assert_eq!(
            restore_window_placement(Some(saved), screens, default),
            (WindowBounds::Maximized(bounds), Some(secondary)),
        );
        assert_eq!(
            restore_window_placement(
                Some(MainWindowPlacement { maximized: false, ..saved }),
                screens,
                default,
            ),
            (WindowBounds::Windowed(bounds), Some(secondary)),
        );
        assert_eq!(
            restore_window_placement(Some(saved), [(primary, screen)], default),
            (WindowBounds::Windowed(default), None),
        );
    }
}
