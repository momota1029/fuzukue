use std::{
    fs, io,
    io::Write as _,
    path::Path,
    sync::Mutex,
};

use fuzukue_view::appearance::AppearanceSettings;
use toml::{Table, Value};

const LAST_WORKSPACE: &str = "last_workspace";
const SAVE_SERVER_ON_CLOSE: &str = "save_server_on_close";
const MODEL_SETUP_PENDING: &str = "model_setup_pending";
// Keep the UI's settings saves and the connection worker's read-modify-writes
// from replacing each other's updates.
static SETTINGS_WRITE: Mutex<()> = Mutex::new(());

pub(super) fn read(path: &Path) -> anyhow::Result<Table> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(toml::from_str(&text)?),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(Table::new()),
        Err(err) => Err(err.into()),
    }
}

fn write(path: &Path, settings: &Table) -> anyhow::Result<()> {
    let dir = path.parent().expect(tr!("settings-settings-file-parent-directory"));
    fs::create_dir_all(dir)?;
    // Readers see either the old settings or the complete new file.
    let mut file = tempfile::NamedTempFile::new_in(dir)?;
    file.write_all(toml::to_string_pretty(settings)?.as_bytes())?;
    file.persist(path)?;
    Ok(())
}

pub(super) fn load_appearance(path: &Path, save_server_default: bool) -> anyhow::Result<AppearanceSettings> {
    let mut table = read(path)?;
    table.entry(SAVE_SERVER_ON_CLOSE).or_insert(Value::Boolean(save_server_default));
    let mut settings: AppearanceSettings = table.try_into()?;
    settings.normalize();
    Ok(settings)
}

pub(super) fn save_appearance(path: &Path, settings: &AppearanceSettings) -> anyhow::Result<()> {
    let _write = SETTINGS_WRITE.lock().unwrap();
    let mut existing = read(path)?;
    merge_appearance(&mut existing, toml::from_str(&settings.to_toml()?)?);
    write(path, &existing)
}

fn merge_appearance(existing: &mut Table, mut appearance: Table) {
    // Each window can save its own placement and appearance. The shutdown
    // preference is shared across the app and written by its toggle.
    appearance.remove(SAVE_SERVER_ON_CLOSE);
    // Serde omits these optional fields. A clear in the UI must remove the
    // previous value while keeping fields understood only by other versions.
    if !appearance.contains_key("main_window") {
        existing.remove("main_window");
    }
    if let (Some(Value::Table(existing)), Some(Value::Table(appearance))) =
        (existing.get_mut("server"), appearance.get("server"))
    {
        for key in ["url", "token"] {
            if !appearance.contains_key(key) {
                existing.remove(key);
            }
        }
    }
    overlay(existing, appearance);
}

fn overlay(existing: &mut Table, updates: Table) {
    for (key, value) in updates {
        match (existing.get_mut(&key), value) {
            (Some(Value::Table(existing)), Value::Table(updates)) => overlay(existing, updates),
            (_, value) => {
                existing.insert(key, value);
            }
        }
    }
}

pub(super) fn load_workspace(path: &Path) -> anyhow::Result<Option<String>> {
    Ok(last_workspace(&read(path)?).map(str::to_owned))
}

pub(super) fn save_workspace(path: &Path, id: &str) -> anyhow::Result<()> {
    update(path, |settings| Ok(remember_workspace(settings, id)))
}

pub(super) fn clear_workspace(path: &Path) -> anyhow::Result<()> {
    update(path, |settings| Ok(settings.remove(LAST_WORKSPACE).is_some()))
}

pub(super) fn save_server_on_close(path: &Path, enabled: bool) -> anyhow::Result<()> {
    update(path, |settings| {
        settings.insert(SAVE_SERVER_ON_CLOSE.into(), Value::Boolean(enabled));
        Ok(true)
    })
}

pub(super) fn initialize_model_setup(path: &Path) -> anyhow::Result<bool> {
    let _write = SETTINGS_WRITE.lock().unwrap();
    let pending = match fs::symlink_metadata(path) {
        Ok(_) => read(path)?.get(MODEL_SETUP_PENDING).and_then(Value::as_bool) == Some(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            // Older clients can have established local state without an appearance file.
            if !matches!(fs::symlink_metadata(path.with_extension("state.toml")),
                Err(error) if error.kind() == io::ErrorKind::NotFound) {
                return Ok(false);
            }
            let mut settings = Table::new();
            settings.insert(MODEL_SETUP_PENDING.into(), Value::Boolean(true));
            write(path, &settings)?;
            true
        }
        Err(error) => return Err(error.into()),
    };
    Ok(pending)
}

pub(super) fn consume_model_setup(path: &Path) -> anyhow::Result<bool> {
    let mut pending = false;
    update(path, |settings| {
        pending = settings.get(MODEL_SETUP_PENDING).and_then(Value::as_bool) == Some(true);
        if pending {
            settings.insert(MODEL_SETUP_PENDING.into(), Value::Boolean(false));
        }
        Ok(pending)
    })?;
    Ok(pending)
}

/// Update a settings document under the same lock as the other writers.
/// The callback returns whether the file needs to be written.
pub(super) fn update(
    path: &Path,
    mutate: impl FnOnce(&mut Table) -> anyhow::Result<bool>,
) -> anyhow::Result<()> {
    let _write = SETTINGS_WRITE.lock().unwrap();
    let mut settings = read(path)?;
    if mutate(&mut settings)? {
        write(path, &settings)?;
    }
    Ok(())
}

fn last_workspace(settings: &Table) -> Option<&str> {
    settings.get(LAST_WORKSPACE)?.as_str().filter(|id| !id.is_empty())
}

fn remember_workspace(settings: &mut Table, id: &str) -> bool {
    if last_workspace(settings) == Some(id) {
        return false;
    }
    settings.insert(LAST_WORKSPACE.into(), Value::String(id.to_owned()));
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use fuzukue_view::appearance::{AccentChoice, AccentPreset, FontWeightChoice, ThemePreference, ThemePreset};

    #[test]
    fn model_setup_round_trip_preserves_existing_settings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");
        let state = path.with_extension("state.toml");
        fs::write(&state, "existing = true\n").unwrap();
        assert!(!initialize_model_setup(&path).unwrap());
        assert!(!path.exists());
        fs::remove_file(&state).unwrap();

        assert!(initialize_model_setup(&path).unwrap());
        save_server_on_close(&path, true).unwrap();
        save_appearance(&path, &AppearanceSettings::default()).unwrap();
        assert!(initialize_model_setup(&path).unwrap());
        assert!(consume_model_setup(&path).unwrap());
        assert!(!consume_model_setup(&path).unwrap());
        assert!(!initialize_model_setup(&path).unwrap());
        assert_eq!(read(&path).unwrap()[SAVE_SERVER_ON_CLOSE].as_bool(), Some(true));
    }

    #[test]
    fn save_server_on_close_defaults_off_and_survives_a_stale_window_save() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");
        let stale = AppearanceSettings::from_toml("").unwrap();
        assert!(!stale.save_server_on_close);
        save_server_on_close(&path, true).unwrap();
        save_appearance(&path, &stale).unwrap();
        let saved = fs::read_to_string(&path).unwrap();
        assert!(AppearanceSettings::from_toml(&saved).unwrap().save_server_on_close);

        save_server_on_close(&path, false).unwrap();
        let saved = fs::read_to_string(&path).unwrap();
        assert!(!AppearanceSettings::from_toml(&saved).unwrap().save_server_on_close);
    }

    #[test]
    fn packaged_shutdown_default_preserves_explicit_choices() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");
        assert!(!load_appearance(&path, false).unwrap().save_server_on_close);
        let settings = load_appearance(&path, true).unwrap();
        assert!(settings.save_server_on_close);
        save_appearance(&path, &settings).unwrap();
        assert!(!read(&path).unwrap().contains_key(SAVE_SERVER_ON_CLOSE));
        assert!(load_appearance(&path, true).unwrap().save_server_on_close);

        save_server_on_close(&path, false).unwrap();
        assert!(!load_appearance(&path, true).unwrap().save_server_on_close);
        save_server_on_close(&path, true).unwrap();
        assert!(load_appearance(&path, false).unwrap().save_server_on_close);
    }

    #[test]
    fn appearance_theme_round_trips_with_unknown_settings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");
        let text = "\
future_key = true
code_font = 'Consolas'
code_font_size_percent = 104
[theme]
base = 'light'
accent = '#128ac0'
code_font = 'Menlo, BIZ UDGothic'
future_palette = 'ocean'
[theme.overrides]
sidebar = '#112233'
";
        fs::write(&path, text).unwrap();
        let mut settings = AppearanceSettings::from_toml(text).unwrap();
        assert_eq!(settings.theme.base, ThemePreference::Light);
        assert_eq!(settings.theme.preset, ThemePreset::Classic);
        assert_eq!(settings.theme.accent, AccentChoice::Custom("#128ac0".into()));
        assert_eq!(settings.theme.code_font, "Menlo, BIZ UDGothic");
        assert_eq!(settings.code_font, "Consolas");
        assert_eq!(settings.code_font_size_percent, 104.0);
        assert_eq!(settings.theme.overrides["sidebar"], "#112233");

        let saved_accent = settings.theme.accent.clone();
        for (preset, base, accent) in [
            (ThemePreset::Iris, ThemePreference::System, saved_accent),
            (ThemePreset::Parchment, ThemePreference::Dark, AccentChoice::Preset(AccentPreset::Purple)),
        ] {
            settings.theme.preset = preset;
            settings.theme.base = base;
            settings.theme.accent = accent;
            save_appearance(&path, &settings).unwrap();
            let saved = fs::read_to_string(&path).unwrap();
            assert_eq!(AppearanceSettings::from_toml(&saved).unwrap(), settings);
            let document: Table = toml::from_str(&saved).unwrap();
            assert_eq!(document["future_key"].as_bool(), Some(true));
            assert_eq!(document["theme"]["future_palette"].as_str(), Some("ocean"));
        }
    }

    #[test]
    fn old_appearance_theme_keeps_its_base_and_defaults_to_classic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");
        for (text, base) in [
            ("future_key = true", ThemePreference::System),
            ("theme = 'dark'\nfuture_key = true", ThemePreference::Dark),
        ] {
            fs::write(&path, text).unwrap();
            let settings = AppearanceSettings::from_toml(text).unwrap();
            assert_eq!(settings.theme.base, base);
            assert_eq!(settings.theme.preset, ThemePreset::Classic);
            assert_eq!(settings.theme.accent, AccentChoice::default());
            assert!(settings.theme.code_font.is_empty());
            assert!(settings.theme.overrides.is_empty());
            save_appearance(&path, &settings).unwrap();
            let saved = fs::read_to_string(&path).unwrap();
            assert_eq!(AppearanceSettings::from_toml(&saved).unwrap(), settings);
            assert_eq!(read(&path).unwrap()["future_key"].as_bool(), Some(true));
        }
    }

    #[test]
    fn font_weights_round_trip_independently_through_the_settings_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");
        let old = "ui_font = 'Yu Gothic UI'\nreading_font = 'Yu Mincho'\ncode_font = 'Consolas'";
        let defaults = AppearanceSettings::from_toml(old).unwrap();
        assert_eq!(defaults.ui_font_weight, FontWeightChoice::Regular);
        assert_eq!(defaults.reading_font_weight, FontWeightChoice::Regular);
        assert_eq!(defaults.code_font_weight, FontWeightChoice::Regular);

        let old_code_weight = format!("{old}\ncode_font_weight = 'medium'");
        fs::write(&path, &old_code_weight).unwrap();
        let mut settings = AppearanceSettings::from_toml(&old_code_weight).unwrap();
        assert_eq!(settings, AppearanceSettings {
            code_font_weight: FontWeightChoice::Medium,
            ..defaults
        });

        let choices = [
            (FontWeightChoice::Light, "light"),
            (FontWeightChoice::Medium, "medium"),
            (FontWeightChoice::Regular, "regular"),
        ];
        for (ui, reading, code) in [(0, 1, 2), (1, 2, 0), (2, 0, 1)] {
            settings.ui_font_weight = choices[ui].0;
            settings.reading_font_weight = choices[reading].0;
            settings.code_font_weight = choices[code].0;
            save_appearance(&path, &settings).unwrap();
            let document = read(&path).unwrap();
            assert_eq!(document["ui_font_weight"].as_str(), Some(choices[ui].1));
            assert_eq!(document["reading_font_weight"].as_str(), Some(choices[reading].1));
            assert_eq!(document["code_font_weight"].as_str(), Some(choices[code].1));
            let saved = fs::read_to_string(&path).unwrap();
            assert_eq!(AppearanceSettings::from_toml(&saved).unwrap(), settings);
        }
    }

    #[test]
    fn workspace_settings_round_trip_preserves_extra_keys() {
        let text = "future_key = true\n[server]\nfuture_server_key = 42\nurl = 'ws://old/ws'\n";
        let mut document: Table = toml::from_str(text).unwrap();
        assert_eq!(last_workspace(&document), None);
        assert!(remember_workspace(&mut document, "fuzukue"));
        assert!(!remember_workspace(&mut document, "fuzukue"));

        let appearance = AppearanceSettings::default();
        merge_appearance(&mut document, toml::from_str(&appearance.to_toml().unwrap()).unwrap());
        let round_trip: Table = toml::from_str(&toml::to_string_pretty(&document).unwrap()).unwrap();
        assert_eq!(last_workspace(&round_trip), Some("fuzukue"));
        assert_eq!(round_trip["future_key"].as_bool(), Some(true));
        assert_eq!(round_trip["server"]["future_server_key"].as_integer(), Some(42));
        assert!(!round_trip["server"].as_table().unwrap().contains_key("url"));
    }
}
