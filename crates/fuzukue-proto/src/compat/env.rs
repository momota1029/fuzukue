// Also included by the two build scripts, which have no runtime dependencies.
use std::collections::HashSet;
use std::ffi::OsString;
use std::sync::{LazyLock, Mutex};

static WARNED: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(Mutex::default);

pub fn legacy_env_name(name: &str) -> Option<String> {
    Some(format!("LABO_{}", name.strip_prefix("FUZUKUE_")?))
}

pub fn env_os(name: &str) -> Option<OsString> {
    env_os_with_warning(name, |message| eprintln!("{message}"))
}

pub fn env_os_with_warning(name: &str, warn: impl FnOnce(String)) -> Option<OsString> {
    std::env::var_os(name).or_else(|| {
        let old = legacy_env_name(name)?;
        let value = std::env::var_os(&old)?;
        if WARNED.lock().unwrap().insert(old.clone()) {
            warn(format!("{old} is deprecated; use {name}"));
        }
        Some(value)
    })
}

#[allow(dead_code)]
pub fn env(name: &str) -> Result<String, std::env::VarError> {
    env_os(name)
        .ok_or(std::env::VarError::NotPresent)?
        .into_string()
        .map_err(std::env::VarError::NotUnicode)
}
