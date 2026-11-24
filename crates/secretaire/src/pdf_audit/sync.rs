use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::File;
use std::io::{self, BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::Duration;

use anyhow::{ensure, Context, Result};
use flate2::read::MultiGzDecoder;
use fuzukue_proto::PdfAuditCheck;
use tokio::process::Command;

use super::{add, check, Fail, Incomplete, Pass};
use crate::synctex::{field, resolve_source, reverse_hit};

pub(super) async fn audit(
    project: &Path,
    pdf: &Path,
    pages: Option<usize>,
    tool: &Path,
) -> PdfAuditCheck {
    let mut result = check(Pass);
    let samples = match sidecar(pdf) {
        Ok(Some(path)) => {
            let location = path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            match tokio::task::spawn_blocking(move || read_sidecar(&path, pages)).await {
                Ok(Ok(samples)) => samples,
                Ok(Err(error)) => {
                    add(
                        &mut result,
                        Fail,
                        "invalid_synctex",
                        &location,
                        format!("{error:#}"),
                    );
                    Vec::new()
                }
                Err(error) => {
                    add(
                        &mut result,
                        Incomplete,
                        "synctex_inspection_failed",
                        &location,
                        error.to_string(),
                    );
                    Vec::new()
                }
            }
        }
        Ok(None) => {
            add(
                &mut result,
                Fail,
                "missing_synctex",
                "SyncTeX",
                "No .synctex or .synctex.gz sidecar",
            );
            Vec::new()
        }
        Err(error) => {
            add(
                &mut result,
                Incomplete,
                "unreadable_synctex",
                "SyncTeX",
                format!("{error:#}"),
            );
            Vec::new()
        }
    };
    // Even with a defective sidecar, distinguish a missing tool from a completed check.
    match run(tool, project, &["help"]).await {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            add(
                &mut result,
                Incomplete,
                "missing_tool",
                "SyncTeX",
                "Install synctex to check forward and reverse source mapping",
            );
            result.findings.last_mut().unwrap().tool = Some("synctex".into());
            return result;
        }
        Err(error) => {
            add(
                &mut result,
                Incomplete,
                "synctex_unavailable",
                "SyncTeX",
                error.to_string(),
            );
            return result;
        }
        Ok(output) if !output.status.success() => {
            add(
                &mut result,
                Incomplete,
                "synctex_unavailable",
                "SyncTeX",
                format!("synctex help exited with {}", output.status),
            );
            return result;
        }
        Ok(_) => {}
    }
    let Some(pages) = pages else {
        add(
            &mut result,
            Incomplete,
            "unknown_pdf_page_count",
            "SyncTeX",
            "Could not confirm the page range against the PDF",
        );
        return result;
    };
    let pdf = match pdf.canonicalize() {
        Ok(pdf) => pdf,
        Err(error) => {
            add(
                &mut result,
                Incomplete,
                "unreadable_pdf",
                "SyncTeX",
                error.to_string(),
            );
            return result;
        }
    };
    for sample in samples {
        let location = format!("Page {}, {}:{}", sample.page, sample.input, sample.line);
        match sample_mapping(project, &pdf, tool, pages, &sample).await {
            Ok(()) => {}
            Err(error) => {
                let status = if error.downcast_ref::<io::Error>().is_some() {
                    Incomplete
                } else {
                    Fail
                };
                add(
                    &mut result,
                    status,
                    "synctex_mapping_failed",
                    &location,
                    format!("{error:#}"),
                );
            }
        }
    }
    result
}

fn sidecar(pdf: &Path) -> Result<Option<PathBuf>> {
    let mut newest = None;
    for ext in ["synctex", "synctex.gz"] {
        let path = pdf.with_extension(ext);
        match path.metadata() {
            Ok(metadata) => {
                let time = metadata.modified().ok();
                if newest.as_ref().is_none_or(|(previous, _)| time > *previous) {
                    newest = Some((time, path));
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| format!("Cannot read {}", path.display()))
            }
        }
    }
    Ok(newest.map(|(_, path)| path))
}

struct Sample {
    page: usize,
    input: String,
    line: u64,
}

fn read_sidecar(path: &Path, page_count: Option<usize>) -> Result<Vec<Sample>> {
    let file = File::open(path)?;
    let reader: Box<dyn BufRead> = if path.extension().is_some_and(|ext| ext == "gz") {
        Box::new(BufReader::new(MultiGzDecoder::new(file)))
    } else {
        Box::new(BufReader::new(file))
    };
    let mut lines = reader.lines();
    let version = lines.next().transpose()?.unwrap_or_default();
    ensure!(
        version
            .strip_prefix("SyncTeX Version:")
            .and_then(|n| n.parse::<u32>().ok())
            .is_some_and(|n| n > 0),
        "Missing or invalid SyncTeX header"
    );
    let mut inputs = HashMap::new();
    let mut sheets = HashSet::new();
    let mut anchors = BTreeMap::new();
    let mut sheet = None;
    let mut content = false;
    let mut postamble = false;
    let mut count = None;
    let mut postscript = false;
    for line in lines {
        let line = line?;
        if postamble {
            if let Some(value) = line.strip_prefix("Count:") {
                count = Some(
                    value
                        .parse::<usize>()
                        .context("Invalid SyncTeX record count")?,
                );
            }
            postscript |= line == "Post scriptum:";
            continue; // Read to EOF to verify gzip integrity.
        }
        if let Some((tag, path)) = line
            .strip_prefix("Input:")
            .and_then(|entry| entry.split_once(':'))
        {
            inputs.insert(tag.parse::<u32>()?, path.to_string());
        } else if line == "Content:" {
            content = true;
        } else if line == "Postamble:" {
            ensure!(content && sheet.is_none(), "SyncTeX content is truncated");
            postamble = true;
        } else if content {
            if let Some(page) = line.strip_prefix('{') {
                let page: usize = page.parse().context("Invalid SyncTeX sheet number")?;
                ensure!(
                    page > 0 && page_count.is_none_or(|count| page <= count),
                    "SyncTeX sheet {page} lies outside the PDF page range"
                );
                ensure!(
                    sheet.is_none() && sheets.insert(page),
                    "Repeated or nested SyncTeX sheet {page}"
                );
                sheet = Some(page);
            } else if let Some(page) = line.strip_prefix('}') {
                ensure!(
                    sheet.is_some() && page.parse::<usize>().ok() == sheet,
                    "Mismatched SyncTeX end of sheet"
                );
                sheet = None;
            } else if let Some(page) = sheet {
                if let Some((tag, line, priority)) = anchor(&line) {
                    let current = anchors.entry(page).or_insert((tag, line, priority));
                    if priority > current.2 {
                        *current = (tag, line, priority);
                    }
                }
            }
        }
    }
    ensure!(
        postscript && count.is_some_and(|n| n > 0) && !sheets.is_empty(),
        "Missing or truncated SyncTeX content or postamble"
    );
    if let Some(count) = page_count {
        ensure!(
            sheets.len() == count,
            "SyncTeX has {} sheets for a {count}-page PDF",
            sheets.len()
        );
    }

    // At most three nonempty sheets, spread through the document.
    let candidates: Vec<_> = anchors
        .into_iter()
        .filter_map(|(page, (tag, line, _))| {
            Some(Sample {
                page,
                line,
                input: inputs.get(&tag)?.clone(),
            })
        })
        .collect();
    ensure!(
        !candidates.is_empty(),
        "SyncTeX contains no usable source anchors"
    );
    let last = candidates.len() - 1;
    Ok(candidates
        .into_iter()
        .enumerate()
        .filter_map(|(index, sample)| [0, last / 2, last].contains(&index).then_some(sample))
        .collect())
}

fn anchor(record: &str) -> Option<(u32, u64, u8)> {
    let priority = match record.as_bytes().first()? {
        b'x' | b'k' | b'g' => 2, // Positions among typeset content.
        b'h' | b'v' | b'(' | b'[' => 1,
        _ => return None,
    };
    let (source, _) = record[1..].split_once(':')?;
    let mut source = source.split(',');
    let tag = source.next()?.parse().ok()?;
    let line = source.next()?.parse().ok()?;
    (line > 0).then_some((tag, line, priority))
}

async fn run(tool: &Path, project: &Path, args: &[&str]) -> io::Result<Output> {
    let mut command = Command::new(tool);
    command
        .args(args)
        .current_dir(project)
        .kill_on_drop(true)
        .env_remove("SYNCTEX_EDITOR")
        .env_remove("SYNCTEX_VIEWER");
    tokio::time::timeout(Duration::from_secs(3), command.output())
        .await
        .map_err(io::Error::other)?
}

async fn sample_mapping(
    project: &Path,
    pdf: &Path,
    tool: &Path,
    pages: usize,
    sample: &Sample,
) -> Result<()> {
    // Older SyncTeX CLIs interpret the optional page hint as part of the file name.
    let input = format!("{}:0:{}", sample.line, sample.input);
    let output = run(
        tool,
        project,
        &["view", "-i", &input, "-o", &pdf.to_string_lossy()],
    )
    .await?;
    ensure!(
        output.status.success(),
        "synctex view exited with {}",
        output.status
    );
    let text = String::from_utf8_lossy(&output.stdout);
    let page = field(&text, "Page:")
        .and_then(|value| value.trim().parse::<usize>().ok())
        .filter(|page| (1..=pages).contains(page))
        .context("Forward mapping returned no valid PDF page")?;
    let coordinate = |key| {
        field(&text, key)
            .and_then(|value| value.trim().parse::<f64>().ok())
            .filter(|value| value.is_finite())
    };
    let x = coordinate("x:").context("Forward mapping returned no valid x coordinate")?;
    let y = coordinate("y:").context("Forward mapping returned no valid y coordinate")?;

    let target = format!("{page}:{x}:{y}:{}", pdf.display());
    let output = run(tool, project, &["edit", "-o", &target]).await?;
    ensure!(
        output.status.success(),
        "synctex edit exited with {}",
        output.status
    );
    let text = String::from_utf8_lossy(&output.stdout);
    let (file, line) = reverse_hit(&text)?;
    let hit = resolve_source(project, pdf, file, line).await?;
    ensure!(
        hit.line > 0 && project.join(&hit.file).is_file(),
        "Reverse mapping did not resolve to an existing source file and line"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;
    use std::os::unix::fs::PermissionsExt;

    const SYNCTEX: &str = include_str!("../../tests/fixtures/pdf-audit/paper.synctex");

    #[tokio::test]
    async fn missing_tool_is_incomplete_even_with_a_readable_sidecar() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let pdf = dir.path().join("paper.pdf");
        std::fs::write(pdf.with_extension("synctex"), SYNCTEX)?;
        let result = audit(dir.path(), &pdf, Some(1), &dir.path().join("missing-tool")).await;
        assert_eq!(result.status, Incomplete);
        assert_eq!(result.findings.len(), 1);
        assert_eq!(result.findings[0].code, "missing_tool");
        assert_eq!(result.findings[0].tool.as_deref(), Some("synctex"));
        Ok(())
    }

    #[tokio::test]
    async fn missing_corrupt_and_out_of_range_sidecars_fail() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let pdf = dir.path().join("paper.pdf");
        std::fs::write(&pdf, crate::pdf_audit::tests::VALID_PDF)?;
        let sync = pdf.with_extension("synctex.gz");
        let tool = dir.path().join("synctex");
        // Only the availability probe should run for absent or malformed sidecars.
        std::fs::write(&tool, "#!/bin/sh\ntest \"$1\" = help\n")?;
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755))?;

        let result = audit(dir.path(), &pdf, Some(1), &tool).await;
        assert_eq!(result.status, Fail, "{result:?}");
        assert_eq!(result.findings[0].code, "missing_synctex");

        std::fs::write(&sync, b"truncated gzip")?;
        let result = audit(dir.path(), &pdf, Some(1), &tool).await;
        assert_eq!(result.status, Fail, "{result:?}");
        assert_eq!(result.findings[0].code, "invalid_synctex");

        std::fs::remove_file(&sync)?;
        std::fs::write(pdf.with_extension("synctex"), SYNCTEX.replace("{1", "{2"))?;
        let result = audit(dir.path(), &pdf, Some(1), &tool).await;
        assert_eq!(result.status, Fail, "{result:?}");
        assert_eq!(result.findings[0].code, "invalid_synctex");
        assert!(result.findings[0]
            .detail
            .contains("outside the PDF page range"));
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires the synctex executable"]
    async fn samples_forward_and_reverse_with_plain_and_gzip_sidecars() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let pdf = dir.path().join("paper.pdf");
        std::fs::write(&pdf, crate::pdf_audit::tests::VALID_PDF)?;
        std::fs::write(pdf.with_extension("tex"), "Source text\n")?;
        let plain = pdf.with_extension("synctex");
        std::fs::write(&plain, SYNCTEX)?;
        let result = audit(dir.path(), &pdf, Some(1), Path::new("synctex")).await;
        assert_eq!(result, check(Pass));

        std::fs::remove_file(plain)?;
        let file = File::create(pdf.with_extension("synctex.gz"))?;
        let mut gzip = flate2::write::GzEncoder::new(file, flate2::Compression::default());
        gzip.write_all(SYNCTEX.as_bytes())?;
        gzip.finish()?;
        let result = audit(dir.path(), &pdf, Some(1), Path::new("synctex")).await;
        assert_eq!(result, check(Pass));
        Ok(())
    }
}
