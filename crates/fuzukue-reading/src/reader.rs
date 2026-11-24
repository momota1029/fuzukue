//! Proof-review export and local private Reader builds.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, ensure, Context, Result};
use serde::Serialize;
use tempfile::{Builder, NamedTempFile};

use crate::review::{self, Language};

const PRIVATE_CONTENT: &str = "website/content/private";
const GENERATOR: &str = "website/generator";

/// Workspace-relative paths of the exported Reader inputs.
#[derive(Serialize)]
pub struct Export {
    markdown: PathBuf,
    diagram: PathBuf,
}

/// Exported inputs and absolute paths to the private preview site and article.
#[derive(Serialize)]
pub struct Build {
    #[serde(flatten)]
    exported: Export,
    site: PathBuf,
    html: PathBuf,
}

fn directory(path: &Path, create: bool) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound && create => {
            fs::create_dir(path).with_context(|| format!("Cannot create {}", path.display()))
        }
        _ => bail!("Expected a regular directory at {}", path.display()),
    }
}

fn generated_file(path: &Path, matches_output: impl FnOnce(&str) -> bool) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Ok(metadata) if metadata.is_file() => {
            let contents = fs::read_to_string(path)
                .with_context(|| format!("Cannot read {}", path.display()))?;
            ensure!(
                matches_output(&contents),
                "Refusing to overwrite a file without the proof-review generator's signature: {}",
                path.display()
            );
            Ok(true)
        }
        _ => bail!("Expected a regular file at {}", path.display()),
    }
}

fn stage_file(path: &Path, contents: &str) -> Result<NamedTempFile> {
    let parent = path.parent().context("Output path has no parent")?;
    let mut staged = NamedTempFile::new_in(parent)?;
    staged.write_all(contents.as_bytes())?;
    Ok(staged)
}

fn install(staged: NamedTempFile, path: &Path, replace: bool) -> Result<()> {
    if replace {
        staged.persist(path)
    } else {
        staged.persist_noclobber(path)
    }
    .with_context(|| format!("Cannot write {}", path.display()))?;
    Ok(())
}

/// Write Reader Markdown and SVG beneath the workspace's existing private content directory.
pub fn export(project: &Path, deck: &Path, slug: &str, language: Language) -> Result<Export> {
    ensure!(
        review::valid_slug(slug),
        "slug must use lowercase ASCII letters, digits and single hyphens"
    );
    let mut content = project.to_path_buf();
    for component in PRIVATE_CONTENT.split('/') {
        content.push(component);
        directory(&content, false).context("Workspace requires website/content/private/")?;
    }

    let rendered = review::deck(deck, slug, language).context("Cannot render proof review")?;
    let markdown = Path::new(PRIVATE_CONTENT).join(format!("{slug}.md"));
    let diagram = Path::new(PRIVATE_CONTENT)
        .join("figures")
        .join(slug)
        .join("diagram.svg");
    let markdown_path = project.join(&markdown);
    let diagram_path = project.join(&diagram);

    // Recognize the lab export's original frontmatter and dated notice in either language.
    let slug_header = format!("slug = \"{slug}\"");
    let notices = [
        Language::Ja.generation_notice(),
        Language::En.generation_notice(),
    ];
    let replace_markdown = generated_file(&markdown_path, |text| {
        let text = text.replace("\r\n", "\n");
        text.split_once("\n+++\n\n")
            .is_some_and(|(frontmatter, body)| {
                frontmatter.starts_with("+++\ntitle = ")
                    && frontmatter.lines().any(|line| line == slug_header)
                    && notices.iter().any(|notice| {
                        body.strip_prefix(notice)
                            .and_then(|rest| rest.lines().next())
                            .is_some_and(|date| {
                                chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d").is_ok()
                            })
                    })
            })
    })?;

    directory(&content.join("figures"), true)?;
    directory(&content.join("figures").join(slug), true)?;
    let replace_diagram = generated_file(&diagram_path, |text| {
        text == rendered.diagram
            || (text.starts_with("<svg width=\"100%\" viewBox=\"0 0 ")
                && text.contains("<defs><marker id=\"ar\" markerWidth=\"8\"")
                && text.contains("<a href=\"#intro\"><rect ")
                && text.trim_end().ends_with("</svg>"))
    })?;

    let staged_markdown = stage_file(&markdown_path, &rendered.markdown)?;
    let staged_diagram = stage_file(&diagram_path, &rendered.diagram)?;
    install(staged_diagram, &diagram_path, replace_diagram)?;
    install(staged_markdown, &markdown_path, replace_markdown)?;
    Ok(Export { markdown, diagram })
}

fn copy_generator(source: &Path, stage: &Path) -> Result<()> {
    directory(source, false)?;
    fs::create_dir_all(stage)?;
    for name in ["Cargo.toml", "Cargo.lock"] {
        let file = source.join(name);
        ensure!(
            fs::symlink_metadata(&file).is_ok_and(|metadata| metadata.is_file()),
            "Expected a regular generator file at {}",
            file.display()
        );
        fs::copy(file, stage.join(name))
            .with_context(|| format!("Cannot stage the workspace's generator {name}"))?;
    }
    for name in ["src", "assets", "templates"] {
        let root = source.join(name);
        directory(&root, false)?;
        for entry in walkdir::WalkDir::new(&root).follow_root_links(false) {
            let entry = entry?;
            let dest = stage.join(name).join(entry.path().strip_prefix(&root)?);
            if entry.file_type().is_dir() {
                fs::create_dir_all(dest)?;
            } else {
                ensure!(
                    entry.file_type().is_file(),
                    "Cannot stage {}",
                    entry.path().display()
                );
                fs::copy(entry.path(), dest)?;
            }
        }
    }
    Ok(())
}

/// Export a page and retain a private build of it beneath the supplied state directory.
///
/// Runs the workspace's Rust site generator with only the selected page and its figure.
pub fn generate(
    project: &Path,
    state: &Path,
    deck: &Path,
    slug: &str,
    language: Language,
) -> Result<Build> {
    let exported = export(project, deck, slug, language)?;

    // The generator resolves its output from its manifest directory at compile time.
    let builds = state.join("reading-page-builds");
    fs::create_dir_all(&builds)?;
    let builds = builds.canonicalize()?;
    let stage = Builder::new().prefix("build-").tempdir_in(&builds)?;
    let website = stage.path().join("website");
    copy_generator(&project.join(GENERATOR), &website.join("generator"))?;

    // The generator resolves translated pages' assets under the base slug.
    let asset_slug = slug.strip_suffix("-en").unwrap_or(slug);
    let assets = stage
        .path()
        .join(PRIVATE_CONTENT)
        .join("figures")
        .join(asset_slug);
    fs::create_dir_all(&assets)?;
    fs::copy(
        project.join(&exported.markdown),
        stage.path().join(&exported.markdown),
    )?;
    fs::copy(project.join(&exported.diagram), assets.join("diagram.svg"))?;

    let output = Command::new("cargo")
        .args([
            "run",
            "--locked",
            "--manifest-path",
            "website/generator/Cargo.toml",
            "--",
            "private",
        ])
        .current_dir(stage.path())
        .env("CARGO_TARGET_DIR", builds.join("target"))
        .output()
        .context("Could not start the private Reader generator (cargo)")?;
    ensure!(
        output.status.success(),
        "Private Reader build failed ({}): {}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim(),
        String::from_utf8_lossy(&output.stdout).trim()
    );

    let article = Path::new("expositions").join(slug).join("index.html");
    let site = website.join("private");
    ensure!(
        site.join(&article).is_file(),
        "Private Reader generator did not produce {}",
        article.display()
    );
    let preview = Builder::new().prefix("reader-").tempdir_in(&builds)?;
    // Retain the generated site and drop the staged sources.
    fs::rename(site, preview.path().join("private"))?;
    let site = preview.keep().join("private");
    let html = site.join(article);
    Ok(Build {
        exported,
        site,
        html,
    })
}
