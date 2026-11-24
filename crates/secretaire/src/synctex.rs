//! PDF の位置から、そこを書いているカードを逆引きする。
//!
//! 1. `pdfinfo` でページの寸法（pt）を得て、正規化座標を pt に直す。
//! 2. `synctex edit -o page:x:y:pdf` で tex のファイルと行を得る。
//! 3. ハーネスのソースマップ、フォルダの `card.md`、または `% card <id>` の目印からカードを得る。

use std::path::Path;

use anyhow::{bail, Context, Result};
use tokio::process::Command;

use crate::build::harness::{map_path, SourceMap};

pub struct Located {
    pub file: String,
    pub line: u64,
    pub card: Option<String>,
}

async fn page_size(pdf: &Path, page: usize) -> Result<(f64, f64)> {
    let p = page.to_string();
    let out = Command::new("pdfinfo").args(["-f", &p, "-l", &p]).arg(pdf).output().await.context("pdfinfo を起動できない")?;
    let text = String::from_utf8_lossy(&out.stdout);
    // "Page    3 size:  595.28 x 841.89 pts (A4)"
    let line = text.lines().find(|l| l.contains("size:")).context("pdfinfo: ページの寸法がない")?;
    let nums: Vec<f64> = line
        .split_once("size:")
        .map(|(_, r)| r)
        .unwrap_or("")
        .split_whitespace()
        .filter_map(|t| t.parse().ok())
        .collect();
    match nums.as_slice() {
        [w, h, ..] => Ok((*w, *h)),
        _ => bail!("pdfinfo の寸法を読めない: {line}"),
    }
}

/// `% card <id>` / `% card <id> (status)` の行から id を取り出す。
pub fn card_marker(line: &str) -> Option<&str> {
    let rest = line.trim_start().strip_prefix('%')?.trim_start().strip_prefix("card ")?;
    rest.split_whitespace().next()
}

/// `page` は 0 始まり（クライアントの表示と同じ）。`nx` / `ny` はページ左上を原点とする 0〜1。
pub async fn locate(project: &Path, pdf_rel: &str, page: usize, nx: f32, ny: f32) -> Result<Located> {
    let pdf = project.join(pdf_rel).canonicalize()?;
    let (w, h) = page_size(&pdf, page + 1).await?;
    let spec = format!("{}:{:.2}:{:.2}:{}", page + 1, nx as f64 * w, ny as f64 * h, pdf.display());
    let out = Command::new("synctex").args(["edit", "-o", &spec]).current_dir(project).output().await.context("synctex を起動できない")?;
    let text = String::from_utf8_lossy(&out.stdout);
    let (file, line) = reverse_hit(&text)?;
    resolve_source(project, &pdf, file, line).await
}

pub(crate) fn reverse_hit(text: &str) -> Result<(&str, u64)> {
    let file = field(text, "Input:").context("synctex: 対応する tex が見つからない（.synctex.gz があるか確かめて）")?;
    let line = field(text, "Line:").and_then(|l| l.trim().parse().ok()).context("synctex: 行がない")?;
    Ok((file, line))
}

pub(crate) fn field<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    text.lines().find_map(|line| line.strip_prefix(key))
}

pub(crate) async fn resolve_source(project: &Path, pdf: &Path, file: &str, line: u64) -> Result<Located> {
    let root = project.canonicalize()?;
    let build = pdf.parent().context("PDF の親ディレクトリがない")?;
    let input = Path::new(file);
    // TeX の作業ディレクトリを基準にした相対パスも SyncTeX に記録される。
    let full = build.join(input).canonicalize()
        .or_else(|_| root.join(input).canonicalize())
        .or_else(|_| build.join("..").join(input).canonicalize())
        .unwrap_or_else(|_| input.to_path_buf());
    if let Ok(input) = full.strip_prefix(build) {
        if let Some(hit) = harness_source(&root, pdf, input, line).await? {
            return Ok(hit);
        }
    }
    // フォルダのカードの .tex なら、その card.md の id。
    let card = if let Some(id) = folder_card(&root, &pdf, &full) {
        Some(id)
    } else {
        tokio::fs::read_to_string(&full)
            .await
            .unwrap_or_default()
            .lines()
            .take(line as usize)
            .fold(None, |card, line| match line.trim() {
                "% end card" => None,
                line => card_marker(line).or(card),
            })
            .map(String::from)
    };
    let file = full.strip_prefix(&root).map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| file.into());
    Ok(Located { file, line, card })
}

async fn harness_source(project: &Path, pdf: &Path, input: &Path, line: u64) -> Result<Option<Located>> {
    let bytes = match tokio::fs::read(map_path(pdf)).await {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    let map: SourceMap = serde_json::from_slice(&bytes).context("ハーネスのソースマップを読めない")?;
    let Some(source) = map.get(input) else { return Ok(None) };
    let deck = pdf.parent().and_then(Path::parent).context("デッキのディレクトリがない")?;
    let full = deck.join(&source.path).canonicalize()?;
    let file = full.strip_prefix(project).context("ハーネスカードがプロジェクトの外にある")?;
    // Suppress synthetic input boundaries by using the nearest source line.
    let line = source.lines.get(line.saturating_sub(1) as usize)
        .or_else(|| source.lines.last()).copied().context("ソースマップに行がない")?;
    Ok(Some(Located { file: file.to_string_lossy().replace('\\', "/"), line, card: Some(source.card.clone()) }))
}

pub(crate) fn folder_card(project: &Path, pdf: &Path, source: &Path) -> Option<String> {
    let frontmatter = std::fs::read_to_string(source.parent()?.join("card.md")).ok()?;
    let (yaml, _) = fuzukue_cards::split_frontmatter(&frontmatter)?;
    let value: serde_yaml::Value = serde_yaml::from_str(yaml).ok()?;
    Some(external_id(project, pdf, source, value["id"].as_str()?.to_owned()))
}

/// 外部カードは PDF を組み立てたデッキでの名前空間つき ID に戻す。
fn external_id(project: &Path, pdf: &Path, source: &Path, id: String) -> String {
    let Some(build) = pdf.parent().filter(|p| p.ends_with("build")) else { return id };
    let Some(deck) = build.parent() else { return id };
    let Ok(cfg) = fuzukue_cards::DeckConfig::read(deck) else { return id };
    let external = cfg.external_roots(project, deck).filter_map(|(namespace, path)| {
        let root = path.ok()?.canonicalize().ok()?;
        if root == deck || (source.starts_with(deck) && !root.starts_with(deck)) {
            return None;
        }
        source.starts_with(&root).then_some((root.components().count(), namespace))
    }).max();
    match external {
        Some((_, namespace)) => format!("{namespace}:{id}"),
        None => id,
    }
}

#[cfg(test)]
mod tests {
    use super::card_marker;

    #[test]
    fn markers() {
        assert_eq!(card_marker("% card x-intro"), Some("x-intro"));
        assert_eq!(card_marker("% card pre-def-prokhorov (lean)"), Some("pre-def-prokhorov"));
        assert_eq!(card_marker("% 概要"), None);
        assert_eq!(card_marker("\\begin{theorem}"), None);
    }
}
