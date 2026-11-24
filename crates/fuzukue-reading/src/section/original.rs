use std::fs;

use image::{codecs::jpeg::JpegEncoder, Rgb, RgbImage};

use super::*;

struct Word {
    x: f64,
    y: f64,
    text: String,
}

struct Page {
    width: f64,
    height: f64,
    words: Vec<Word>,
}

pub(super) struct Original<'a> {
    path: &'a Path,
    pages: Vec<Page>,
}

impl<'a> Original<'a> {
    pub(super) fn read(path: &'a Path) -> Result<Self> {
        let result = command(Command::new("pdftotext").arg("-bbox").arg(path).arg("-"))?;
        let text = String::from_utf8(result)?;
        let page = Regex::new(r#"(?s)<page width="([\d.]+)" height="([\d.]+)">(.*?)</page>"#)?;
        let word = Regex::new(
            r#"(?s)<word xMin="([\d.]+)" yMin="([\d.]+)" xMax="[^"]+" yMax="[^"]+">(.*?)</word>"#,
        )?;
        let pages = page
            .captures_iter(&text)
            .map(|p| {
                let content = p.get(3).unwrap().as_str();
                let words = word
                    .captures_iter(content)
                    .map(|w| {
                        Ok(Word {
                            x: w[1].parse()?,
                            y: w[2].parse()?,
                            text: w[3]
                                .replace("&quot;", "\"")
                                .replace("&apos;", "'")
                                .replace("&lt;", "<")
                                .replace("&gt;", ">")
                                .replace("&amp;", "&"),
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
                Ok(Page {
                    width: p[1].parse()?,
                    height: p[2].parse()?,
                    words,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        ensure!(
            !pages.is_empty(),
            "no page bounds read from {}",
            path.display()
        );
        Ok(Self { path, pages })
    }

    fn locate(&self, s: &Value) -> Option<(usize, f64)> {
        let (a, b) = range(&s["pages"])?;
        let heading = string(s, "heading_word");
        let id = string(s, "id");
        if heading.is_empty() {
            return None;
        }
        let start = usize::try_from(a - 1).ok()?.min(self.pages.len());
        let end = usize::try_from(b)
            .ok()?
            .saturating_add(1)
            .min(self.pages.len());
        (start..end).find_map(|p| {
            self.pages[p]
                .words
                .windows(2)
                .find(|w| w[0].text == id && w[0].x < 140.0 && w[1].text == heading)
                .map(|w| (p, w[0].y))
        })
    }

    pub(super) fn crop(&self, s: &Value, next: Option<&Value>) -> Result<(String, String)> {
        let Some((first, y)) = self.locate(s) else {
            return Ok((String::new(), "heading not located".into()));
        };
        let next_heading = next
            .and_then(|s| self.locate(s))
            .filter(|(page, y1)| (*page, *y1) > (first, y));
        let (last, end) = next_heading.unwrap_or_else(|| {
            let recorded_end = range(&s["pages"]).unwrap().1;
            let last = (recorded_end - 1).min(self.pages.len() as u64 - 1) as usize;
            let last = last.max(first);
            (last, self.pages[last].height)
        });
        let tmp = tempfile::tempdir()?;
        let mut images = Vec::new();
        for p in first..=last {
            let page = &self.pages[p];
            let n = (p + 1).to_string();
            let y0 = if p == first { (y - 6.0).max(0.0) } else { 70.0 };
            let y1 = if p == last {
                let padding = if next_heading.is_some() { 6.0 } else { 0.0 };
                (end - padding).min(page.height)
            } else {
                706.0_f64.min(page.height)
            };
            if y1 - y0 < 14.0 {
                continue;
            }
            let scale = 110.0 / 72.0;
            let x = 60.0_f64.min(page.width / 10.0) * scale;
            let width = (480.0 * scale).min(page.width * scale - x);
            let prefix = tmp.path().join(&n);
            command(
                Command::new("pdftoppm")
                    .args([
                        "-singlefile",
                        "-r",
                        "110",
                        "-f",
                        &n,
                        "-l",
                        &n,
                        "-x",
                        &(x as u32).to_string(),
                        "-y",
                        &((y0 * scale) as u32).to_string(),
                        "-W",
                        &(width as u32).to_string(),
                        "-H",
                        &(((y1 - y0) * scale) as u32).to_string(),
                        "-jpeg",
                    ])
                    .arg(self.path)
                    .arg(&prefix),
            )?;
            let bytes = fs::read(prefix.with_extension("jpg"))?;
            images.push((
                p + 1,
                image::load_from_memory_with_format(&bytes, image::ImageFormat::Jpeg)?.into_rgb8(),
            ));
        }
        if images.is_empty() {
            return Ok((String::new(), "empty region".into()));
        }
        let height = images
            .iter()
            .try_fold(0u32, |h, (_, img)| h.checked_add(img.height()))
            .context("crop too tall")?;
        let width = images.iter().map(|(_, img)| img.width()).max().unwrap();
        let mut combined = RgbImage::from_pixel(width, height, Rgb([255, 255, 255]));
        let mut y = 0;
        let mut breaks = String::new();
        for (page, image) in &images {
            image::imageops::replace(&mut combined, image, 0, i64::from(y));
            if y > 0 {
                for x in (0..width).filter(|x| x % 12 < 6) {
                    combined.put_pixel(x, y, Rgb([120, 140, 160]));
                }
                let top = 100.0 * f64::from(y) / f64::from(height);
                write!(breaks, r#"<span style="position:absolute;top:{top:.3}%;right:0;padding:0 .35em;background:#ebf1f7;color:#3c5064;font-size:.8em">p.{page}</span>"#)
                    .unwrap();
            }
            y += image.height();
        }
        let mut jpeg = Vec::new();
        JpegEncoder::new_with_quality(&mut jpeg, 72).encode_image(&combined)?;
        let page = images[0].0;
        let last_page = images.last().unwrap().0;
        let label = if last_page == page {
            format!("p.{page}")
        } else {
            format!("p.{page}-{last_page}")
        };
        let image = format!(
            r#"<img src="{}" alt="p.{page}">"#,
            data_uri("image/jpeg", &jpeg)
        );
        let image = if breaks.is_empty() {
            image
        } else {
            format!(
                r#"<div style="display:inline-block;position:relative;max-width:100%;line-height:1">{image}{breaks}</div>"#
            )
        };
        let end_description = if next_heading.is_some() {
            "the next heading".into()
        } else {
            format!("the end of p.{last_page}")
        };
        let id = string(s, "id");
        Ok((
            format!(
                r#"<div class="origscroll"><figure>{image}<figcaption>{label} (original, this section only; page breaks marked by dotted lines)</figcaption></figure></div><button class="small" onclick="this.previousElementSibling.classList.toggle('full')">expand / collapse the original column</button>"#
            ),
            format!(
                "from the heading of §{id} on p.{} to {end_description}, as one continuous region",
                first + 1
            ),
        ))
    }
}

pub(super) struct Region {
    pub translation: String,
    pub image: String,
    pub description: String,
}

impl Region {
    pub(super) fn render(&self, reuse: Option<&str>) -> String {
        let image = if self.image.is_empty() {
            r#"<p class="muted">no original region (give --original and heading_word)</p>"#.into()
        } else if let Some(id) = reuse {
            // Keep the JPEG in one place. The reading view points at its corresponding deep dive.
            Regex::new(r#"<img src="[^"]*""#)
                .unwrap()
                .replace(&self.image, format!(r#"<img data-same="{id}""#))
                .into_owned()
        } else {
            self.image.clone()
        };
        let translation = if self.translation.is_empty() {
            "<p class=muted>(none)</p>"
        } else {
            &self.translation
        };
        format!(
            r#"<div class="otgrid"><div><h4>Original</h4><p class="small muted">{}</p>{image}</div><div><h4>Faithful translation</h4>{translation}</div></div>"#,
            escape(&self.description)
        )
    }
}

pub(super) fn regions(r: &Record, pdf: Option<&Path>) -> Result<BTreeMap<String, Region>> {
    let original = pdf.map(Original::read).transpose()?;
    r.sections()
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let file = string(s, "translation_html");
            let translation = if file.is_empty() {
                list(&s["translation"])
                    .iter()
                    .map(|p| format!("<p>{}</p>", html(strval(p))))
                    .collect()
            } else {
                read(&r.base.join(file))?
            };
            let (image, description) =
                if let Some(original) = original.as_ref().filter(|_| !blank(&s["heading_word"])) {
                    original.crop(s, r.sections().get(i + 1))?
                } else {
                    (String::new(), String::new())
                };
            Ok((
                string(s, "id").to_owned(),
                Region {
                    translation,
                    image,
                    description,
                },
            ))
        })
        .collect()
}
