use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs;
use std::path::Path;
use std::process::{Command, Output};
use std::sync::LazyLock;

use anyhow::{bail, ensure, Context, Result};
use regex::Regex;
use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

use crate::DeckConfig;

use super::scan::{argument, tokens};

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Engine {
    #[serde(rename = "uplatex-dvipdfmx")]
    UpLaTeX,
    #[serde(skip)]
    PLaTeX,
    PdfLaTeX,
    LuaLaTeX,
    XeLaTeX,
}

impl Engine {
    pub fn name(self) -> &'static str {
        match self {
            Self::UpLaTeX => "uplatex-dvipdfmx",
            Self::PLaTeX => "platex-dvipdfmx",
            Self::PdfLaTeX => "pdflatex",
            Self::LuaLaTeX => "lualatex",
            Self::XeLaTeX => "xelatex",
        }
    }

    pub(super) fn detect(tex: &str, language: Option<&str>) -> Result<Self> {
        static PROGRAM: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(r"(?im)^[ \t]*%\s*!\s*TeX\s+(?:TS-)?program\s*=\s*([A-Za-z]+)").unwrap()
        });
        let explicit = PROGRAM.captures(tex).map(|m| m[1].to_ascii_lowercase());
        Ok(match explicit.as_deref() {
            Some("uplatex") => Self::UpLaTeX,
            Some("platex") => Self::PLaTeX,
            Some("pdflatex") => Self::PdfLaTeX,
            Some("lualatex") => Self::LuaLaTeX,
            Some("xelatex") => Self::XeLaTeX,
            Some(engine) => bail!("TeX program に対応していない ({engine})"),
            None if tex.contains("uplatex") => Self::UpLaTeX,
            None if tex.contains("ltjs") || tex.contains("luatexja") => Self::LuaLaTeX,
            None if language == Some("ja") => Self::UpLaTeX,
            None => Self::PdfLaTeX,
        })
    }

    pub fn args(self) -> &'static [&'static str] {
        match self {
            Self::UpLaTeX => &["-pdfdvi", "-e", "$latex = 'uplatex %O %S'; $bibtex = 'upbibtex %O %B'; $dvipdf = 'dvipdfmx %O -o %D %S';"],
            Self::PLaTeX => &["-pdfdvi", "-e", "$latex = 'platex %O %S'; $bibtex = 'pbibtex %O %B'; $dvipdf = 'dvipdfmx %O -o %D %S';"],
            Self::PdfLaTeX => &["-pdf"],
            Self::LuaLaTeX => &["-lualatex"],
            Self::XeLaTeX => &["-xelatex"],
        }
    }

    pub const COMMON_ARGS: &'static [&'static str] = &[
        "-norc",
        "-no-shell-escape",
        "-interaction=nonstopmode",
        "-halt-on-error",
        "-synctex=1",
    ];
}

fn compile(cwd: &Path, input: &Path, out: &Path, engine: Engine) -> Result<Output> {
    Ok(Command::new("latexmk")
        .args(Engine::COMMON_ARGS)
        .args(engine.args())
        .arg(format!("-outdir={}", out.display()))
        .arg(input)
        .current_dir(cwd)
        .output()?)
}

pub(super) fn build_deck(deck: &Path, engine: Engine) -> Result<()> {
    let run = compile(
        deck,
        Path::new("build/paper.tex"),
        Path::new("build"),
        engine,
    )
    .context("latexmk を起動できない")?;
    ensure!(run.status.success(), "{}", log_tail(&run));
    Ok(())
}

fn log_tail(run: &Output) -> String {
    let text =
        String::from_utf8_lossy(&run.stdout).to_string() + &String::from_utf8_lossy(&run.stderr);
    let lines: Vec<_> = text.lines().rev().take(20).collect();
    lines.into_iter().rev().collect::<Vec<_>>().join("\n    ")
}

fn labels(out: &Path) -> Result<BTreeMap<String, String>> {
    let mut labels = BTreeMap::new();
    for file in WalkDir::new(out).sort_by_file_name() {
        let file = file?;
        if file.path().extension().is_none_or(|ext| ext != "aux") {
            continue;
        }
        let text = fs::read_to_string(file.path())?;
        for token in tokens(&text) {
            if &text[token.clone()] != "\\newlabel" {
                continue;
            }
            if let Some((key, end)) = argument(&text, token.end, '{', '}') {
                if let Some((value, _)) = argument(&text, end, '{', '}') {
                    labels.insert(key.into(), value.into());
                }
            }
        }
    }
    Ok(labels)
}

fn undefined(log: &Path) -> Result<BTreeSet<String>> {
    let log = fs::read(log)?;
    Ok(String::from_utf8_lossy(&log)
        .lines()
        .map(str::trim)
        .filter(|line| line.contains("undefined") && line.contains("Warning:"))
        .map(str::to_owned)
        .collect())
}

fn extract(pdf: &Path) -> Result<Option<String>> {
    match Command::new("pdftotext")
        .args(["-enc", "UTF-8"])
        .arg(pdf)
        .arg("-")
        .output()
    {
        Ok(run) if run.status.success() => Ok(Some(String::from_utf8(run.stdout)?)),
        Ok(run) => bail!(
            "pdftotext に失敗した ({})",
            String::from_utf8_lossy(&run.stderr)
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

pub(super) fn compare(main: &Path, deck: &Path, report: &mut String) -> Result<String> {
    report.push_str("\n## PDF の比較\n\n");
    let cfg = DeckConfig::read(deck)?;
    let engine = super::prepare_build(deck, &cfg)?;
    let original = tempfile::tempdir()?;
    let main_dir = main.parent().context("主ファイルのディレクトリがない")?;
    let main_name = Path::new(main.file_name().context("主ファイル名がない")?);
    let run = match compile(main_dir, main_name, original.path(), engine) {
        Ok(run) => run,
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
        {
            report.push_str("latexmk が見つからなかったため省略した。\n");
            return Ok("latexmk なし".into());
        }
        Err(error) => return Err(error),
    };
    let deck_out = deck.join("build");
    let imported = compile(
        deck,
        Path::new("build/paper.tex"),
        Path::new("build"),
        engine,
    )?;
    if !run.status.success() || !imported.status.success() {
        for (name, run) in [("原本", &run), ("デッキ", &imported)] {
            if !run.status.success() {
                writeln!(
                    report,
                    "- {name} のビルドに失敗した\n\n    {}\n",
                    log_tail(run)
                )?;
            }
        }
        return Ok("ビルド失敗".into());
    }
    let mut same = true;
    let before = labels(original.path())?;
    let after = labels(&deck_out)?;
    let keys: BTreeSet<_> = before.keys().chain(after.keys()).collect();
    let mismatches: Vec<_> = keys
        .iter()
        .filter(|&&key| before.get(key) != after.get(key))
        .collect();
    writeln!(
        report,
        "- AUX ラベル {} 件（番号・ページ・参照情報）、差分 {} 件",
        keys.len(),
        mismatches.len()
    )?;
    same &= mismatches.is_empty();
    for &&key in &mismatches {
        writeln!(
            report,
            "  - {key} 原本 {:?} / デッキ {:?}",
            before.get(key),
            after.get(key)
        )?;
    }
    let missing_before = undefined(&original.path().join(main_name.with_extension("log")))?;
    let missing_after = undefined(&deck_out.join("paper.log"))?;
    same &= missing_before.is_empty() && missing_after.is_empty();
    writeln!(
        report,
        "- 未解決参照の警告（重複除去） 原本 {} 件 / デッキ {} 件",
        missing_before.len(),
        missing_after.len()
    )?;
    for (name, missing) in [("原本", missing_before), ("デッキ", missing_after)] {
        for warning in missing {
            writeln!(report, "  - {name} {warning}")?;
        }
    }
    let original_pdf = original.path().join(main_name.with_extension("pdf"));
    let Some(original_text) = extract(&original_pdf)? else {
        report.push_str("- pdftotext が見つからなかったため、抽出テキストの比較を省略した。\n");
        return Ok(if same {
            "参照一致・本文未確認"
        } else {
            "参照に差分または警告あり"
        }
        .into());
    };
    let imported_text =
        extract(&deck_out.join("paper.pdf"))?.context("pdftotext が見つからない")?;
    let left: Vec<_> = original_text.split_whitespace().collect();
    let right: Vec<_> = imported_text.split_whitespace().collect();
    if left == right {
        report.push_str("- PDF 抽出テキストは空白の違いを除いて一致した。\n");
    } else {
        same = false;
        let first = left
            .iter()
            .zip(&right)
            .position(|(a, b)| a != b)
            .unwrap_or(left.len().min(right.len()));
        writeln!(
            report,
            "- PDF 抽出テキストに差分がある。最初の差分は空白区切りの {} 項目目。",
            first + 1
        )?;
        for (name, tokens) in [("原本", left), ("デッキ", right)] {
            let start = first.saturating_sub(4);
            let end = (first + 8).min(tokens.len());
            writeln!(
                report,
                "  - {name}（{} 項目） {}",
                tokens.len(),
                tokens[start..end].join(" ")
            )?;
        }
    }
    Ok(if same {
        "一致"
    } else {
        "差分または警告あり"
    }
    .into())
}
