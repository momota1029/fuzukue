use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::Command;

use anyhow::{ensure, Context, Result};
use base64::Engine;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

pub fn read(path: &Path) -> Result<String> {
    fs::read_to_string(path)
        .with_context(|| format!("read {}", path.display()))
        .map(|text| text.replace("\r\n", "\n").replace('\r', "\n"))
}

pub fn json(path: &Path) -> Result<Value> {
    serde_json::from_str(&read(path)?).with_context(|| format!("parse {}", path.display()))
}

pub fn write(path: &Path, data: impl AsRef<[u8]>) -> Result<()> {
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(dir)?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(data.as_ref())?;
    tmp.persist(path)
        .with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

pub fn sha(data: impl AsRef<[u8]>) -> String {
    format!("{:x}", Sha256::digest(data.as_ref()))
}

pub fn data_uri(mime: &str, data: &[u8]) -> String {
    format!(
        "data:{mime};base64,{}",
        base64::engine::general_purpose::STANDARD.encode(data)
    )
}

pub fn escape(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#x27;")
}

pub fn strval(value: &Value) -> &str {
    value.as_str().unwrap_or_default()
}

pub fn string<'a>(value: &'a Value, key: &str) -> &'a str {
    strval(&value[key])
}

pub fn list(value: &Value) -> &[Value] {
    value.as_array().map(Vec::as_slice).unwrap_or_default()
}

pub fn members(value: &Value) -> impl Iterator<Item = (&String, &Value)> {
    value.as_object().into_iter().flatten()
}

pub fn truth(value: &Value) -> bool {
    match value {
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_f64() != Some(0.0),
        _ => !blank(value),
    }
}

pub fn blank(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::String(value) => value.trim().is_empty(),
        Value::Array(value) => value.is_empty(),
        Value::Object(value) => value.is_empty(),
        _ => false,
    }
}

pub fn with(value: &Value, additions: impl IntoIterator<Item = (&'static str, Value)>) -> Value {
    let mut object: Map<String, Value> = value.as_object().cloned().unwrap_or_default();
    object.extend(
        additions
            .into_iter()
            .map(|(key, value)| (key.into(), value)),
    );
    Value::Object(object)
}

pub fn selected(value: &Value, keys: &[&'static str]) -> Value {
    with(
        &Value::Null,
        keys.iter().map(|&key| (key, value[key].clone())),
    )
}

pub fn id(value: &str) -> Result<&str> {
    ensure!(
        !value.is_empty()
            && value != "."
            && value != ".."
            && value
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')),
        "invalid identifier {value:?}"
    );
    Ok(value)
}

pub fn executable(name: &str) -> Option<std::path::PathBuf> {
    let path = Path::new(name);
    if path.is_absolute() {
        return path.is_file().then(|| path.to_owned());
    }
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(path))
        .find(|path| path.is_file())
}

pub fn command(command: &mut Command) -> Result<Vec<u8>> {
    let result = command
        .output()
        .with_context(|| format!("run {command:?}"))?;
    ensure!(
        result.status.success(),
        "{command:?} exited {}\n{}\n{}",
        result.status,
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    Ok(result.stdout)
}

pub fn file_url(path: &Path) -> Result<String> {
    #[cfg(windows)]
    let prefix = "file:///";
    #[cfg(not(windows))]
    let prefix = "file://";
    use std::fmt::Write;
    let absolute = std::path::absolute(path)?;
    let path = absolute.to_string_lossy().replace('\\', "/");
    let mut url = String::from(prefix);
    for byte in path.bytes() {
        if byte.is_ascii_alphanumeric() || b"/-._~:".contains(&byte) {
            url.push(char::from(byte));
        } else {
            write!(url, "%{byte:02X}")?;
        }
    }
    Ok(url)
}
