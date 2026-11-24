//! Build clickable PDF regions from SyncTeX horizontal boxes. Vertical boxes can
//! span several cards (or an entire sheet), so their source is not a hit target.

use std::collections::{BTreeMap, HashMap};
use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result};
use flate2::read::GzDecoder;
use fuzukue_proto::PdfCardRegion;
use sha2::{Digest, Sha256};

use crate::build::harness::{map_path, SourceMap};
use crate::synctex::{card_marker, folder_card};

#[derive(Default)]
struct Page {
    media: [f64; 4],
    crop: [f64; 4],
    rotation: u16,
}

impl Page {
    fn normalize(&self, [x0, y0, x1, y1]: [f64; 4]) -> Option<[f32; 4]> {
        let [l, b, r, t] = self.crop;
        let w = r - l;
        let h = t - b;
        if w <= 0. || h <= 0. {
            return None;
        }
        let x0 = ((x0 + self.media[0] - l) / w).clamp(0., 1.);
        let x1 = ((x1 + self.media[0] - l) / w).clamp(0., 1.);
        let y0 = ((y0 + t - self.media[3]) / h).clamp(0., 1.);
        let y1 = ((y1 + t - self.media[3]) / h).clamp(0., 1.);
        if x1 <= x0 || y1 <= y0 {
            return None;
        }
        let (x, y, w, h) = match self.rotation {
            90 => (1. - y1, x0, y1 - y0, x1 - x0),
            180 => (1. - x1, 1. - y1, x1 - x0, y1 - y0),
            270 => (y0, 1. - x1, y1 - y0, x1 - x0),
            _ => (x0, y0, x1 - x0, y1 - y0),
        };
        Some([x as f32, y as f32, w as f32, h as f32])
    }
}

fn pages(info: &str) -> HashMap<usize, Page> {
    let mut pages = HashMap::<_, Page>::new();
    for line in info.lines() {
        let mut fields = line.split_whitespace();
        if fields.next() != Some("Page") {
            continue;
        }
        let Some(page) = fields
            .next()
            .and_then(|n| n.parse::<usize>().ok())
            .filter(|n| *n > 0)
        else {
            continue;
        };
        let page = pages.entry(page - 1).or_default();
        match fields.next() {
            Some("rot:") => page.rotation = fields.next().and_then(|n| n.parse().ok()).unwrap_or(0),
            Some(key @ ("MediaBox:" | "CropBox:")) => {
                let rect = fields.map(str::parse::<f64>).collect::<Result<Vec<_>, _>>();
                if let Ok(rect) = rect {
                    if let Ok(rect) = rect.try_into() {
                        if key == "MediaBox:" {
                            page.media = rect;
                        } else {
                            page.crop = rect;
                        }
                    }
                }
            }
            _ => {}
        }
    }
    pages
}

enum Input {
    Card(String, bool),
    Generated(Vec<Option<String>>),
}

impl Input {
    fn card(&self, line: usize) -> Option<(&str, bool)> {
        match self {
            Self::Card(id, proof) => Some((id, *proof)),
            Self::Generated(lines) => lines
                .get(line.checked_sub(1)?)?
                .as_deref()
                .map(|id| (id, false)),
        }
    }
}

fn input(project: &Path, pdf: &Path, sources: &SourceMap, name: &str) -> Option<Input> {
    let build = pdf.parent()?;
    let path = build
        .join(name)
        .canonicalize()
        .or_else(|_| project.join(name).canonicalize())
        .ok()?;
    if !path.starts_with(project) || path.extension()?.to_str()? != "tex" {
        return None;
    }
    let name = path.file_name()?.to_str()?;
    let proof = name == "proof.tex"
        || name.ends_with(".proof.tex")
        || name.ends_with(".deferred-proof.tex");
    if let Some(source) = path.strip_prefix(build).ok().and_then(|p| sources.get(p)) {
        return Some(Input::Card(source.card.clone(), proof));
    }
    if let Some(card) = folder_card(project, pdf, &path) {
        return Some(Input::Card(card, proof));
    }
    if path != pdf.with_extension("tex") {
        return None;
    }
    let mut card = None;
    let lines = std::fs::read_to_string(path)
        .ok()?
        .lines()
        .map(|line| {
            let line = line.trim_start();
            if line == "% end card"
                || line.starts_with(r"\end{document}")
                || line.starts_with(r"\section{")
                || line.starts_with(r"\subsection{")
            {
                card = None;
            } else if let Some(id) = card_marker(line) {
                card = Some(id.to_owned());
            }
            card.clone()
        })
        .collect();
    Some(Input::Generated(lines))
}

fn numbers<const N: usize>(raw: &str) -> Option<[f64; N]> {
    let mut values = [0.0_f64; N];
    for (value, raw) in values.iter_mut().zip(raw.split(',')) {
        *value = raw.parse().ok()?;
        if !value.is_finite() {
            return None;
        }
    }
    (raw.split(',').count() == N).then_some(values)
}

fn regions(
    sync: &str,
    pages: &HashMap<usize, Page>,
    mut resolve: impl FnMut(&str) -> Option<Input>,
) -> Vec<PdfCardRegion> {
    // SyncTeX stores scaled TeX points; PDF uses 72 points per inch.
    const SP: f64 = 72. / (72.27 * 65536.);
    let mut inputs = HashMap::new();
    let mut page = None;
    let (mut unit, mut mag, mut x_offset, mut y_offset) = (1., 1000., 0., 0.);
    let mut boxes = BTreeMap::<_, Vec<[f64; 4]>>::new();
    for line in sync.lines() {
        if let Some((tag, file)) = line.strip_prefix("Input:").and_then(|s| s.split_once(':')) {
            if let Some(source) = resolve(file) {
                inputs.insert(tag, source);
            }
        } else if let Some(sheet) = line.strip_prefix('{') {
            page = sheet.parse::<usize>().ok().and_then(|n| n.checked_sub(1));
        } else if line.starts_with('}') {
            page = None;
        } else if let Some((key, value)) = line.split_once(':') {
            let target = match key {
                "Unit" => Some(&mut unit),
                "Magnification" => Some(&mut mag),
                "X Offset" => Some(&mut x_offset),
                "Y Offset" => Some(&mut y_offset),
                _ => None,
            };
            if let Some(target) = target {
                *target = value.trim().parse().unwrap_or(*target);
            }
            let Some((page, source)) = page.zip(key.strip_prefix('(')) else {
                continue;
            };
            let Some((tag, line)) = source.split_once(',') else {
                continue;
            };
            // Column tracking is optional. The first component after the tag is the line.
            let Some((card, proof)) = line
                .split(',')
                .next()
                .and_then(|n| n.parse().ok())
                .and_then(|line| inputs.get(tag)?.card(line))
            else {
                continue;
            };
            let Some((position, dimensions)) = value.split_once(':') else {
                continue;
            };
            let Some([x, y]) = numbers(position) else {
                continue;
            };
            let Some([w, h, depth]) = numbers(dimensions) else {
                continue;
            };
            if w <= 0. || h + depth <= 0. {
                continue;
            }
            let scale = SP * unit * mag / 1000.;
            let x = x * scale + SP * unit * x_offset;
            let y = y * scale + SP * unit * y_offset;
            boxes
                .entry((page, card.to_owned(), proof))
                .or_default()
                .push([
                    x - 2.,
                    y - h * scale - 2.,
                    x + w * scale + 2.,
                    y + depth * scale + 2.,
                ]);
        }
    }
    let mut result = Vec::new();
    for ((page, card, proof), mut rects) in boxes {
        let Some(dim) = pages.get(&page) else {
            continue;
        };
        rects.sort_by(|a, b| a[1].total_cmp(&b[1]));
        let mut merged: Vec<[f64; 4]> = Vec::new();
        for rect in rects {
            // Join nearby lines in a column, preserving gaps and separate columns.
            if let Some(last) = merged
                .last_mut()
                .filter(|last| rect[1] <= last[3] + 12. && rect[0] < last[2] && rect[2] > last[0])
            {
                last[0] = last[0].min(rect[0]);
                last[2] = last[2].max(rect[2]);
                last[3] = last[3].max(rect[3]);
            } else {
                merged.push(rect);
            }
        }
        result.extend(merged.into_iter().filter_map(|rect| {
            Some(PdfCardRegion {
                page,
                card: card.clone(),
                proof,
                rect: dim.normalize(rect)?,
            })
        }));
    }
    result
}

fn read_regions(project: &Path, pdf: &Path) -> Result<Vec<PdfCardRegion>> {
    let gz = pdf.with_extension("synctex.gz");
    let plain = pdf.with_extension("synctex");
    let sync_path = if gz.is_file() { gz } else { plain };
    // A failed compile can update SyncTeX without producing a new PDF.
    let pdf_time = pdf.metadata()?.modified()?;
    if !sync_path.is_file()
        || sync_path.metadata()?.modified()? > pdf_time
        || pdf
            .with_extension("tex")
            .metadata()
            .and_then(|meta| meta.modified())
            .is_ok_and(|time| time > pdf_time)
    {
        return Ok(Vec::new());
    }
    let mut bytes = Vec::new();
    if sync_path.extension().is_some_and(|ext| ext == "gz") {
        GzDecoder::new(std::fs::File::open(sync_path)?).read_to_end(&mut bytes)?;
    } else {
        bytes = std::fs::read(sync_path)?;
    }
    let info = std::process::Command::new("pdfinfo")
        .env("LC_ALL", "C")
        .args(["-f", "1", "-l", "2147483647", "-box"])
        .arg(pdf)
        .output()
        .context("pdfinfo を起動できない")?;
    anyhow::ensure!(
        info.status.success(),
        "pdfinfo: {}",
        String::from_utf8_lossy(&info.stderr)
    );
    let pages = pages(&String::from_utf8_lossy(&info.stdout));
    let sources = std::fs::read(map_path(pdf))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default();
    Ok(regions(&String::from_utf8_lossy(&bytes), &pages, |name| {
        input(project, pdf, &sources, name)
    }))
}

pub(crate) fn read(project: &Path, pdf: &Path) -> Result<(String, Vec<PdfCardRegion>)> {
    let bytes = std::fs::read(pdf)?;
    let digest = format!("{:x}", Sha256::digest(&bytes));
    let regions = read_regions(project, pdf).unwrap_or_else(|err| {
        tracing::warn!("PDF カード対応 {}: {err:#}", pdf.display());
        Vec::new()
    });
    Ok((digest, regions))
}
