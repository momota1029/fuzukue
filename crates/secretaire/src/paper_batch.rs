//! ワークスペースの原稿を一時コピー上でビルドし、デッキごとの診断をまとめる。

use std::fmt::Write as _;
use std::fs;
use std::io::Write as _;
use std::path::{Component, Path, PathBuf};
use std::process::Output;
use std::sync::{Arc, LazyLock};

use anyhow::{ensure, Context, Result};
use fuzukue_cards::{workspace_paths, DeckConfig};
use fuzukue_i18n::{FluentArgs, Locale};
use regex::Regex;
use serde::Serialize;

use super::BuildLog;
use crate::cancellation::Cancellation;

#[derive(Default, Serialize)]
pub(super) struct Diagnostics {
    undefined_references: usize,
    undefined_citations: usize,
    box_warnings: usize,
    #[serde(skip)]
    warnings: bool,
    first_error: Option<String>,
}

impl Diagnostics {
    pub(super) fn record(&mut self, run: &Output, log_path: &Path) {
        let stdout = String::from_utf8_lossy(&run.stdout);
        let stderr = String::from_utf8_lossy(&run.stderr);
        let log = fs::read(log_path).ok();
        let log = log.as_deref().map(String::from_utf8_lossy);
        self.warnings(log.as_deref().unwrap_or_else(|| final_tex_pass(&stdout)));
        self.warnings |= stderr.lines().chain(stdout.lines()).any(|line| {
            [
                "dvipdfmx:warning:",
                "xdvipdfmx:warning:",
                "Warning--",
                "WARN -",
            ]
            .iter()
            .any(|prefix| line.trim_start().starts_with(prefix))
        });
        if !run.status.success() && self.first_error.is_none() {
            let error = log
                .as_deref()
                .and_then(first_error)
                .or_else(|| first_error(&stdout))
                .or_else(|| first_error(&stderr))
                .or_else(|| {
                    stdout
                        .lines()
                        .find(|line| line.starts_with("Latexmk: Failure"))
                })
                .or_else(|| stderr.lines().find(|line| !line.trim().is_empty()))
                .unwrap_or("latexmk failed without an error message");
            self.error(error.trim().to_owned());
        }
    }

    fn warnings(&mut self, log: &str) {
        static WARNING: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(r"(?i)^(?:latex(?: font)?|package \S+|class \S+) warning: (.*)").unwrap()
        });
        let mut lines = log.lines().peekable();
        while let Some(line) = lines.next() {
            if line
                .strip_prefix("Overfull ")
                .or_else(|| line.strip_prefix("Underfull "))
                .is_some_and(|rest| rest.starts_with("\\hbox") || rest.starts_with("\\vbox"))
            {
                self.box_warnings += 1;
                self.warnings = true;
            }
            if let Some(warning) = WARNING.captures(line) {
                self.warnings = true;
                let mut warning = warning[1].to_ascii_lowercase();
                let reference =
                    warning.starts_with("reference ") || warning.starts_with("hyper reference ");
                let citation = warning.starts_with("citation ");
                if reference || citation {
                    // Long keys and messages can wrap at TeX's log line limit.
                    while !warning.contains("undefined")
                        && lines.peek().is_some_and(|line| {
                            !line.trim().is_empty()
                                && !WARNING.is_match(line)
                                && !["Overfull ", "Underfull ", "! "]
                                    .iter()
                                    .any(|prefix| line.starts_with(prefix))
                        })
                    {
                        warning.push_str(&lines.next().unwrap().trim().to_ascii_lowercase());
                    }
                    if warning.contains("undefined") {
                        self.undefined_references += usize::from(reference);
                        self.undefined_citations += usize::from(citation);
                    }
                }
            }
        }
    }

    pub(super) fn error(&mut self, error: String) {
        self.first_error.get_or_insert(error);
    }
}

fn final_tex_pass(stdout: &str) -> &str {
    static PASS: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?m)^Run number \d+ of rule '(?:pdf|lua|xe)?latex(?: [^']+)?'").unwrap()
    });
    PASS.find_iter(stdout)
        .last()
        .map_or(stdout, |pass| &stdout[pass.end()..])
}

fn first_error(log: &str) -> Option<&str> {
    static ERROR: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(concat!(
            r"(?i)^(?:!\s|.+:\d+:\s|(?:error|fatal)(?::|\s+-\s+)|",
            r".*(?:couldn't open (?:database|style) file|:\s*(?:error|fatal):|: not found$)|",
            r"I found no .+ command)"
        ))
        .unwrap()
    });
    log.lines()
        .map(str::trim_start)
        .find(|line| ERROR.is_match(line))
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum Status {
    Success,
    Warnings,
    Failed,
}

impl Status {
    fn label(&self) -> &'static str {
        match self {
            Self::Success => harness_text!("Success", "成功"),
            Self::Warnings => harness_text!("Warnings", "警告つき"),
            Self::Failed => harness_text!("Failed", "失敗"),
        }
    }
}

#[derive(Serialize)]
struct Row {
    deck: String,
    status: Status,
    engine: Option<&'static str>,
    #[serde(flatten)]
    diagnostics: Diagnostics,
}

#[derive(Serialize)]
pub(crate) struct Report {
    summary: Summary,
    rows: Vec<Row>,
    pub(crate) markdown: String,
    pub(crate) output: Option<String>,
    pub(crate) output_error: Option<String>,
}

const FAILURE_LIMIT: usize = 5;

#[derive(Default, Serialize)]
struct Summary {
    total: usize,
    success: usize,
    warnings: usize,
    failed: usize,
    failed_papers: Vec<FailedPaper>,
}

#[derive(Serialize)]
struct FailedPaper {
    name: String,
    first_error: Option<String>,
}

impl Summary {
    fn from_rows(rows: &[Row]) -> Self {
        let mut summary = Self {
            total: rows.len(),
            ..Self::default()
        };
        for row in rows {
            match row.status {
                Status::Success => summary.success += 1,
                Status::Warnings => summary.warnings += 1,
                Status::Failed => {
                    summary.failed += 1;
                    if summary.failed_papers.len() < FAILURE_LIMIT {
                        summary.failed_papers.push(FailedPaper {
                            name: excerpt(&row.deck, 80),
                            first_error: row
                                .diagnostics
                                .first_error
                                .as_deref()
                                .map(|error| excerpt(error, 160)),
                        });
                    }
                }
            }
        }
        summary
    }

    fn heading(&self) -> String {
        let locale = harness_text!(Locale::supported("en").unwrap(), Locale::JAPANESE);
        let mut args = FluentArgs::new();
        args.set("total", self.total.to_string());
        args.set("success", self.success.to_string());
        args.set("warnings", self.warnings.to_string());
        args.set("failed", self.failed.to_string());
        locale.format("paper-batch-summary", &args)
    }
}

fn excerpt(text: &str, limit: usize) -> String {
    let mut chars = text.chars();
    let mut line: String = chars
        .by_ref()
        .take(limit)
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect();
    if chars.next().is_some() {
        line.pop();
        line.push('…');
    }
    line
}

fn markdown(summary: &Summary, rows: &[Row]) -> String {
    fn cell(text: &str) -> String {
        text.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('\\', "&#92;")
            .replace('|', "&#124;")
            .replace(['\r', '\n'], " ")
    }
    fn list_text(text: &str) -> String {
        let mut escaped = String::new();
        for ch in text.chars() {
            if matches!(
                ch,
                '\\' | '`' | '*' | '_' | '[' | ']' | '<' | '>' | '&' | '~' | '$' | '#' | '|'
            ) {
                escaped.push('\\');
            }
            escaped.push(ch);
        }
        escaped
    }

    let mut table = summary.heading();
    table.push_str("\n\n");
    for paper in &summary.failed_papers {
        let name = list_text(&paper.name);
        let error = list_text(paper.first_error.as_deref().unwrap_or("—"));
        writeln!(table, "- {name}: {error}").unwrap();
    }
    if !summary.failed_papers.is_empty() {
        table.push('\n');
    }
    table.push_str(harness_text!(
        "| Deck | Status | Engine | Undefined references | Undefined citations | Box warnings | First error |\n",
        "| デッキ | 状態 | エンジン | 未定義参照数 | 未定義引用数 | 行あふれ数 | 最初のエラー |\n"
    ));
    table.push_str("| --- | --- | --- | ---: | ---: | ---: | --- |\n");
    for row in rows {
        let diagnostics = &row.diagnostics;
        writeln!(
            table,
            "| {} | {} | {} | {} | {} | {} | {} |",
            cell(&row.deck),
            row.status.label(),
            row.engine.unwrap_or("—"),
            diagnostics.undefined_references,
            diagnostics.undefined_citations,
            diagnostics.box_warnings,
            cell(diagnostics.first_error.as_deref().unwrap_or("—"))
        )
        .unwrap();
    }
    table
}

// Keep the workspace layout for TeX inputs, local latexmkrc files and external
// harness references. Internal symlinks must also point into the copy.
pub(crate) fn copy_workspace(source: &Path, destination: &Path, cancel: &Cancellation) -> Result<()> {
    let source = source.canonicalize()?;
    for entry in walkdir::WalkDir::new(&source)
        .into_iter()
        .filter_entry(|entry| {
            entry.depth() == 0
                || (Some(entry.path()) != destination.parent()
                    && !matches!(
                        entry.file_name().to_str(),
                        Some(".git" | "target" | ".lake" | "node_modules" | ".temp")
                    ))
        })
    {
        cancel.check()?;
        let entry = entry?;
        let to = destination.join(entry.path().strip_prefix(&source)?);
        let kind = entry.file_type();
        if kind.is_dir() {
            fs::create_dir_all(&to)?;
        } else if kind.is_file() {
            fs::copy(entry.path(), &to)
                .with_context(|| format!("{} をコピーできない", entry.path().display()))?;
        } else if kind.is_symlink() {
            if let Ok(target) = entry.path().canonicalize() {
                if let Ok(path) = target.strip_prefix(&source) {
                    let relative = workspace_paths::relative(
                        to.parent().context("symlink has no parent")?,
                        &destination.join(path),
                    )?;
                    let relative = if relative.as_os_str().is_empty() {
                        Path::new(".")
                    } else {
                        &relative
                    };
                    std::os::unix::fs::symlink(relative, &to)?;
                }
            }
        }
    }
    Ok(())
}

fn output_path(project: &Path, output: &str) -> Result<PathBuf> {
    let path = Path::new(output);
    ensure!(
        path.file_name().is_some()
            && path
                .components()
                .all(|part| { matches!(part, Component::Normal(_) | Component::CurDir) }),
        "output はワークスペース相対のファイルパスで指定する"
    );
    let path = workspace_paths::resolve(project, project, path)?;
    ensure!(!path.is_dir(), "output はファイルを指定する");
    Ok(path)
}

fn write_report(project: &Path, output: &str, markdown: &str) -> Result<()> {
    let path = output_path(project, output)?;
    let parent = path.parent().context("output の親ディレクトリがない")?;
    fs::create_dir_all(parent)?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.write_all(markdown.as_bytes())?;
    crate::durable::persist(file, path)?;
    Ok(())
}

pub(crate) async fn run(
    project: &Path,
    output: Option<String>,
    cancel: Arc<Cancellation>,
) -> Result<Report> {
    if let Some(output) = &output {
        output_path(project, output)?;
    }
    let temporary = tempfile::Builder::new()
        .prefix("fuzukue-paper-build-")
        .tempdir()?;
    let snapshot = temporary.path().canonicalize()?.join("workspace");
    let (source, destination, copy_cancel) =
        (project.to_path_buf(), snapshot.clone(), cancel.clone());
    tokio::task::spawn_blocking(move || copy_workspace(&source, &destination, &copy_cancel))
        .await??;

    let mut rows = Vec::new();
    for deck in fuzukue_cards::find_decks(&snapshot) {
        cancel.check()?;
        if DeckConfig::read_in_workspace(&deck, &snapshot).is_ok_and(|cfg| {
            !(crate::workspace::is_paper(&deck, &cfg)
                || matches!(cfg.meta.kind.as_str(), "book" | "slides" | "document")
                || cfg.meta.imported_latex
                || cfg.meta.build.is_some_and(|build| build.tex_root.is_some())
                || deck.join(cfg.structure).is_file())
        }) {
            continue;
        }
        let deck_rel = deck.strip_prefix(&snapshot)?;
        let deck_name = if deck_rel.as_os_str().is_empty() {
            ".".into()
        } else {
            deck_rel.to_string_lossy()
        };
        let mut log = BuildLog {
            diagnostics: Some(Diagnostics::default()),
            ..BuildLog::default()
        };
        let result = super::build_deck(&snapshot, &deck_name, &deck, &cancel, &mut log).await;
        cancel.check()?;
        let mut diagnostics = log.diagnostics.unwrap_or_default();
        let status = match result {
            Ok(output) if output.ok && diagnostics.warnings => Status::Warnings,
            Ok(output) if output.ok => Status::Success,
            Ok(_) => Status::Failed,
            Err(error) => {
                diagnostics.error(format!("{error:#}"));
                Status::Failed
            }
        };
        if let Some(error) = &mut diagnostics.first_error {
            *error = error.replace(snapshot.to_string_lossy().as_ref(), ".");
        }
        rows.push(Row {
            deck: deck_name.into_owned(),
            status,
            engine: log.engine,
            diagnostics,
        });
    }
    let summary = Summary::from_rows(&rows);
    let markdown = markdown(&summary, &rows);
    // Remove intermediate files before publishing the report.
    tokio::task::spawn_blocking(move || temporary.close()).await??;
    cancel.check()?;
    let (output, output_error) = match output {
        Some(output) => match write_report(project, &output, &markdown) {
            Ok(()) => (Some(output), None),
            Err(error) => (None, Some(format!("{output}: {error:#}"))),
        },
        None => (None, None),
    };
    Ok(Report {
        summary,
        rows,
        markdown,
        output,
        output_error,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;

    #[test]
    fn diagnostics_use_the_final_log_and_keep_the_first_error_across_targets() -> Result<()> {
        let dir = crate::test_support::TestDir::new();
        dir.write(
            "target.log",
            r"LaTeX Warning: Reference 'first' on page 1 undefined on input line 3.
Package natbib Warning: Citation 'an-especially-long-key' on page 1 undef
ined on input line 3.
LaTeX Warning: There were undefined references.
Underfull \vbox (badness 2000) has occurred while \output is active
Overfull \hbox (2.0pt too wide) in paragraph at lines 4--7
./a|b.tex:17: Undefined control sequence.
! Another error.
",
        );
        let failed = Output {
            status: std::process::ExitStatus::from_raw(1 << 8),
            stdout: b"LaTeX Warning: Reference 'from a previous pass' undefined.\n".to_vec(),
            stderr: Vec::new(),
        };
        let mut diagnostics = Diagnostics::default();
        diagnostics.record(&failed, &dir.path().join("target.log"));
        assert_eq!(diagnostics.undefined_references, 1);
        assert_eq!(diagnostics.undefined_citations, 1);
        assert_eq!(diagnostics.box_warnings, 2);
        assert!(diagnostics.warnings);
        diagnostics.record(&failed, &dir.path().join("missing.log"));
        assert_eq!(diagnostics.undefined_references, 2);
        assert_eq!(
            diagnostics.first_error.as_deref(),
            Some("./a|b.tex:17: Undefined control sequence.")
        );
        let rows = [Row {
            deck: "a|b".into(),
            status: Status::Failed,
            engine: Some("pdflatex"),
            diagnostics,
        }];
        let table = markdown(&Summary::from_rows(&rows), &rows);
        assert!(table.contains("a&#124;b.tex:17"));
        assert!(table.contains("| a&#124;b |"));
        Ok(())
    }

    #[test]
    fn diagnostics_fall_back_to_the_last_tex_pass_and_converter_error() {
        let run = Output {
            status: std::process::ExitStatus::from_raw(1 << 8),
            stdout: b"Run number 1 of rule 'latex'
LaTeX Warning: Reference 'resolved-on-next-pass' undefined.
Latexmk: Missing input file 'main.nav' (or dependence on it) from following:
Run number 2 of rule 'latex'
LaTeX Warning: Citation 'unresolved' undefined.
"
            .to_vec(),
            stderr: b"dvipdfmx:fatal: Cannot proceed without a font\n".to_vec(),
        };
        let dir = crate::test_support::TestDir::new();
        let mut diagnostics = Diagnostics::default();
        diagnostics.record(&run, &dir.path().join("missing.log"));
        assert_eq!(diagnostics.undefined_references, 0);
        assert_eq!(diagnostics.undefined_citations, 1);
        assert_eq!(
            diagnostics.first_error.as_deref(),
            Some("dvipdfmx:fatal: Cannot proceed without a font")
        );
    }

    #[test]
    fn summary_keeps_five_bounded_entries_before_large_rows() {
        let name = "<".repeat(500);
        let error = format!("line one\n{}", "[".repeat(500));
        let rows: Vec<_> = (0..6)
            .map(|index| Row {
                deck: format!("{index}-{name}"),
                status: Status::Failed,
                engine: None,
                diagnostics: Diagnostics {
                    first_error: Some(error.clone()),
                    ..Default::default()
                },
            })
            .collect();
        let summary = Summary::from_rows(&rows);
        assert_eq!(summary.failed, 6);
        assert_eq!(summary.failed_papers.len(), 5);
        let last = &summary.failed_papers[4];
        assert!(last.name.starts_with("4-"));
        assert_eq!(last.name.chars().count(), 80);
        let first_error = last.first_error.as_deref().unwrap();
        assert!(first_error.starts_with("line one ["));
        assert!(first_error.ends_with('…'));
        assert_eq!(first_error.chars().count(), 160);

        let report = markdown(&summary, &rows);
        assert!(report.chars().count() > 4_000);
        let preview: String = report.chars().take(4_000).collect();
        assert_eq!(
            preview
                .lines()
                .filter(|line| line.starts_with("- "))
                .count(),
            5
        );
        assert!(preview.contains("\n\n| "));
    }
}
