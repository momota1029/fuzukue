use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context as _, Result};
use serde_json::{json, Value};

use super::{mcp_app, mcp_call, mcp_data, TempProject};

const DECK: &str = "reading/review";
const CONTENT: &str = "website/content/private";

fn review_spec(project: &Path) -> Result<Value> {
    let deck = project.join(DECK);
    fs::create_dir_all(&deck)?;
    fs::write(deck.join("deck.toml"), "[meta]\ntitle = \"Proof review\"\n")?;
    fs::write(
        deck.join("steps.json"),
        json!({
            "steps": [
                {"id": "T", "title": "Result", "statement": "Result.", "deps": ["S1"],
                 "loc": "Theorem 1", "origin": "new", "tier": "standard", "role": "goal"},
                {"id": "S1", "title": "An <estimate>", "statement": "An estimate.", "deps": [],
                 "loc": "Lemma 1", "origin": "adapted", "tier": "key", "role": "bound | limit",
                 "why_tier": "The estimate is needed for the result."}
            ]
        })
        .to_string(),
    )?;
    fs::write(
        deck.join("bodies.json"),
        json!({"S1": {
            "l1": "<p>Set <span class=\"m\">x_1 &lt; y</span>.</p>",
            "l2": "<p>Use <b>the bound</b>.</p>",
            "l3": "<div class=\"d\">x_1 = y - 1</div>"
        }})
        .to_string(),
    )?;
    let axes = [
        "correctness",
        "novelty",
        "delta",
        "strength",
        "significance",
        "literature",
    ]
    .map(|key| json!({"key": key, "level": "unknown", "why": "Still to check."}));
    let review = json!({
        "title": "A \"quoted\" theorem",
        "summary_html": "<p>Reading note.</p>",
        "review_conclusion": "A bounded claim.",
        "panel": [{"name": "Scope", "who": "Reader", "scope": "Lemma 1", "result": "Checked"}],
        "findings": [{"id": "F1", "sev": "minor", "status": "confirmed", "steps": ["S1"],
                      "from": ["Reader"], "short": "Add a locator", "detail": "<p>See Lemma 1.</p>"}],
        "prior": [],
        "indicator": {
            "axes": axes,
            "verdict": "none",
            "verdict_why": "Review in progress.",
            "changers": ["Complete the review."]
        }
    });
    fs::write(deck.join("review.json"), review.to_string())?;
    Ok(review)
}

#[tokio::test]
async fn reading_page_export_writes_markdown_and_svg_and_refreshes_generated_files() -> Result<()> {
    let project = TempProject::new()?;
    let mut review = review_spec(project.path())?;
    let dest = project.path().join(CONTENT);
    fs::create_dir_all(&dest)?;
    let app = mcp_app(&project);
    let args = json!({"deck": DECK, "slug": "review", "lang": "ja"});
    let result = mcp_call(&app, "export_reading_page", args.clone()).await;
    let data = mcp_data(&result);
    assert_eq!(data["markdown"], format!("{CONTENT}/review.md"));
    assert_eq!(
        data["diagram"],
        format!("{CONTENT}/figures/review/diagram.svg")
    );
    let page = dest.join("review.md");
    let svg = dest.join("figures/review/diagram.svg");
    let markdown = fs::read_to_string(&page)?;
    let nested = markdown.split_once("{#step-S1}").unwrap().1;
    assert!(nested.contains("$x_1 < y$"), "{nested}");
    assert!(nested.contains("::: details 要点\n\nUse **the bound**.\n\n:::"));
    assert!(nested.contains("::: details 全行の証明\n\n$$\nx_1 = y - 1\n$$\n\n:::"));
    assert!(markdown.contains("title = \"A \\\"quoted\\\" theorem\""));
    assert!(markdown.contains("![依存関係図](asset:diagram.svg)"));
    assert!(markdown.contains("| bound \\| limit |"));
    assert!(markdown.contains("[F1](#find-F1)"));
    assert!(markdown.contains("{#find-F1}"));
    let original = fs::read_to_string(&svg)?;
    assert!(original.starts_with("<svg width=\"100%\" viewBox="));
    assert!(original.contains("An &lt;estimat"));
    assert!(original.contains("marker-end=\"url(#ar)\""));

    fs::write(dest.join("unrelated.md"), "handwritten")?;
    fs::write(dest.join("figures/review/other.svg"), "handwritten SVG")?;
    review["summary_html"] = json!("<p>Updated.</p>");
    fs::write(
        project.path().join(DECK).join("review.json"),
        review.to_string(),
    )?;
    let result = mcp_call(&app, "export_reading_page", args).await;
    mcp_data(&result);
    assert!(fs::read_to_string(&page)?.contains("Updated."));
    assert_eq!(fs::read_to_string(&svg)?, original);
    assert_eq!(
        fs::read_to_string(dest.join("unrelated.md"))?,
        "handwritten"
    );
    assert_eq!(
        fs::read_to_string(dest.join("figures/review/other.svg"))?,
        "handwritten SVG"
    );
    Ok(())
}

#[tokio::test]
async fn reading_page_export_refuses_missing_site_and_handwritten_output() -> Result<()> {
    let project = TempProject::new()?;
    review_spec(project.path())?;
    let app = mcp_app(&project);
    let args = json!({"deck": DECK, "slug": "review", "lang": "en"});
    let result = mcp_call(&app, "export_reading_page", args.clone()).await;
    assert_eq!(result["isError"], true);
    assert!(result
        .to_string()
        .contains("requires website/content/private/"));
    assert!(!project.path().join("website").exists());

    let dest = project.path().join(CONTENT);
    fs::create_dir_all(&dest)?;
    let invalid = mcp_call(
        &app,
        "export_reading_page",
        json!({"deck": DECK, "slug": "../review", "lang": "en"}),
    )
    .await;
    assert_eq!(invalid["isError"], true);
    assert!(invalid.to_string().contains("slug must use"));

    let page = dest.join("review.md");
    fs::write(&page, "handwritten")?;
    let result = mcp_call(&app, "export_reading_page", args.clone()).await;
    assert_eq!(result["isError"], true);
    assert!(result.to_string().contains("Refusing to overwrite"));
    assert_eq!(fs::read_to_string(&page)?, "handwritten");
    assert!(!dest.join("figures").exists());

    fs::remove_file(&page)?;
    fs::create_dir_all(dest.join("figures/review"))?;
    let diagram = dest.join("figures/review/diagram.svg");
    fs::write(&diagram, "<svg>handwritten</svg>")?;
    let result = mcp_call(&app, "export_reading_page", args).await;
    assert_eq!(result["isError"], true);
    assert!(result.to_string().contains("Refusing to overwrite"));
    assert!(!page.exists());
    assert_eq!(fs::read_to_string(diagram)?, "<svg>handwritten</svg>");
    Ok(())
}

#[tokio::test]
async fn reading_page_export_rejects_invalid_review_before_writing() -> Result<()> {
    let project = TempProject::new()?;
    let mut review = review_spec(project.path())?;
    review["indicator"]["verdict"] = json!("journal");
    fs::write(
        project.path().join(DECK).join("review.json"),
        review.to_string(),
    )?;
    let dest = project.path().join(CONTENT);
    fs::create_dir_all(&dest)?;
    let result = mcp_call(
        &mcp_app(&project),
        "export_reading_page",
        json!({"deck": DECK, "slug": "review", "lang": "en"}),
    )
    .await;
    assert_eq!(result["isError"], true);
    assert!(result.to_string().contains("indicator violates the rules"));
    assert_eq!(fs::read_dir(dest)?.count(), 0);
    Ok(())
}

#[tokio::test]
async fn reading_page_export_rejects_dependency_cycles_before_writing() -> Result<()> {
    let project = TempProject::new()?;
    review_spec(project.path())?;
    let ledger_path = project.path().join(DECK).join("steps.json");
    let mut ledger: Value = serde_json::from_slice(&fs::read(&ledger_path)?)?;
    ledger["steps"][1]["deps"] = json!(["T"]);
    fs::write(ledger_path, serde_json::to_vec(&ledger)?)?;

    let dest = project.path().join(CONTENT);
    fs::create_dir_all(&dest)?;
    let result = mcp_call(
        &mcp_app(&project),
        "export_reading_page",
        json!({"deck": DECK, "slug": "review", "lang": "en"}),
    )
    .await;
    assert_eq!(result["isError"], true);
    assert!(result.to_string().contains("dependency cycle"));
    assert_eq!(fs::read_dir(dest)?.count(), 0);
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn reading_page_export_refuses_a_symlinked_content_directory() -> Result<()> {
    let project = TempProject::new()?;
    review_spec(project.path())?;
    let other = TempProject::new()?;
    fs::create_dir_all(project.path().join("website/content"))?;
    std::os::unix::fs::symlink(other.path(), project.path().join(CONTENT))?;

    let result = mcp_call(
        &mcp_app(&project),
        "export_reading_page",
        json!({"deck": DECK, "slug": "review", "lang": "en"}),
    )
    .await;
    assert_eq!(result["isError"], true);
    assert!(result
        .to_string()
        .contains("requires website/content/private/"));
    assert_eq!(fs::read_dir(other.path())?.count(), 0);
    Ok(())
}

#[tokio::test]
async fn reading_page_generator_runs_private_mode_with_staged_input() -> Result<()> {
    let project = TempProject::new()?;
    review_spec(project.path())?;
    let website = project.path().join("website");
    for dir in [
        "content/private",
        "generator/src",
        "generator/assets",
        "generator/templates",
        "private",
        "public",
    ] {
        fs::create_dir_all(website.join(dir))?;
    }
    fs::write(website.join("private/index.html"), "live Reader")?;
    fs::write(website.join("public/index.html"), "public site")?;
    fs::write(
        website.join("generator/Cargo.toml"),
        "[package]\nname = \"paper-site\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n",
    )?;
    fs::write(
        website.join("generator/Cargo.lock"),
        "version = 4\n\n[[package]]\nname = \"paper-site\"\nversion = \"0.1.0\"\n",
    )?;
    fs::write(
        website.join("generator/src/main.rs"),
        r#"
use std::{env, fs, path::Path};

fn main() {
    assert_eq!(env::args().skip(1).collect::<Vec<_>>(), ["private"]);
    let website = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let markdown = fs::read_to_string(website.join("content/private/review-en.md")).unwrap();
    assert!(markdown.contains("language = \"en\""));
    assert!(markdown.contains("![Dependency diagram](asset:diagram.svg)"));
    assert!(website.join("content/private/figures/review/diagram.svg").is_file());
    let article = website.join("private/expositions/review-en");
    fs::create_dir_all(&article).unwrap();
    fs::write(article.join("index.html"), "<article>fixture Reader</article>").unwrap();
}
"#,
    )?;
    let result = mcp_call(
        &mcp_app(&project),
        "generate_reading_page",
        json!({"deck": DECK, "slug": "review-en", "lang": "en"}),
    )
    .await;
    let data = mcp_data(&result);
    assert_eq!(
        data["diagram"],
        "website/content/private/figures/review-en/diagram.svg"
    );
    let html = Path::new(data["html"].as_str().unwrap());
    assert!(html.starts_with(
        project
            .path()
            .join(".fuzukue/reading-page-builds")
            .canonicalize()?
    ));
    assert_eq!(
        fs::read_to_string(html)?,
        "<article>fixture Reader</article>"
    );
    assert_eq!(
        fs::read_to_string(website.join("private/index.html"))?,
        "live Reader"
    );
    assert_eq!(
        fs::read_to_string(website.join("public/index.html"))?,
        "public site"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires copied Poincaré beta inputs and the lab's Reader generator"]
async fn reading_page_poincare_beta_parity_and_private_reader() -> Result<()> {
    use chrono::{Local, NaiveDate};
    use sha2::{Digest, Sha256};

    let lab = PathBuf::from(
        std::env::var_os("FUZUKUE_READING_PAGE_LAB").context("set FUZUKUE_READING_PAGE_LAB")?,
    );
    const SLUG: &str = "poincare-beta-proof-review";
    const SVG: &str = "b3c3bfff08167e66e1580ccef95ad1fd6b69d0e2e984909076f3608b902ce223";
    let deck = format!("reading/{SLUG}");
    let project = TempProject::new()?;
    let root = project.path();
    fs::create_dir_all(root.join(&deck))?;
    for file in ["steps.json", "bodies.json", "review.json"] {
        fs::copy(lab.join(&deck).join(file), root.join(&deck).join(file))?;
    }
    let references = std::env::var_os("FUZUKUE_READING_PAGE_REFERENCE").map(PathBuf::from);
    let generator = lab.join("website/generator");
    let dest = root.join("website/generator");
    fs::create_dir_all(&dest)?;
    for file in ["Cargo.toml", "Cargo.lock"] {
        fs::copy(generator.join(file), dest.join(file))?;
    }
    for dir in ["src", "assets", "templates"] {
        for entry in walkdir::WalkDir::new(generator.join(dir)) {
            let entry = entry?;
            let target = dest.join(entry.path().strip_prefix(&generator)?);
            if entry.file_type().is_dir() {
                fs::create_dir_all(target)?;
            } else {
                fs::copy(entry.path(), target)?;
            }
        }
    }
    fs::create_dir_all(root.join(CONTENT).join("figures").join(SLUG))?;
    let markdown = format!("{SLUG}.md");
    let svg = format!("figures/{SLUG}/diagram.svg");
    // Replace the existing generated Japanese page from the lab.
    for file in [&markdown, &svg] {
        fs::copy(lab.join(CONTENT).join(file), root.join(CONTENT).join(file))?;
    }

    let app = mcp_app(&project);
    let english_slug = format!("{SLUG}-en");
    // The reference Python exporter wrote these SHA-256 values on 2026-11-23.
    // Normalize only the date line and separately check the current date.
    for (tool, lang, slug, date_prefix, hash) in [
        (
            "generate_reading_page",
            "ja",
            SLUG,
            "\n> 生成日 ",
            "4bb8ede34241e69371cf384e436ac95707489df98f5f5d6d72493038bc8dfb82",
        ),
        (
            "export_reading_page",
            "en",
            english_slug.as_str(),
            "\n> Generated ",
            "b0ea81a7202292087781d4b095713aeab82af0f04db8c8edabe4f43b66bd0532",
        ),
    ] {
        let before = Local::now().date_naive();
        let result = mcp_call(
            &app,
            tool,
            json!({"deck": deck, "slug": slug, "lang": lang}),
        )
        .await;
        let data = mcp_data(&result);
        let markdown_path = root.join(data["markdown"].as_str().unwrap());
        let diagram = fs::read(root.join(data["diagram"].as_str().unwrap()))?;
        let text = fs::read_to_string(markdown_path)?;
        if let Some(reference) = references.as_ref().map(|root| root.join(lang)) {
            assert_eq!(
                text.as_bytes(),
                fs::read(reference.join(format!("{slug}.md")))?,
                "{lang} current Markdown"
            );
            assert_eq!(
                diagram,
                fs::read(reference.join(&svg))?,
                "{lang} current SVG"
            );
        }
        let (front, rest) = text
            .split_once(date_prefix)
            .context("missing generated date")?;
        let (date, body) = rest.split_once('\n').context("missing body after date")?;
        let parsed = NaiveDate::parse_from_str(date, "%Y-%m-%d")?;
        assert!((before..=Local::now().date_naive()).contains(&parsed));
        assert_eq!(date, parsed.format("%Y-%m-%d").to_string());
        let normalized = format!("{front}{date_prefix}2026-11-23\n{body}");
        assert_eq!(
            format!("{:x}", Sha256::digest(normalized.as_bytes())),
            hash,
            "{lang} Markdown"
        );
        assert_eq!(format!("{:x}", Sha256::digest(&diagram)), SVG, "{lang} SVG");

        if tool == "generate_reading_page" {
            let site = Path::new(data["site"].as_str().unwrap());
            let article = site.join(format!("expositions/{slug}/index.html"));
            assert_eq!(Path::new(data["html"].as_str().unwrap()), article);
            let html = fs::read_to_string(&article)?;
            for marker in [
                "<details",
                "<table",
                "class=\"katex",
                "id=\"step-",
                "id=\"find-",
                &svg,
            ] {
                assert!(html.contains(marker), "missing {marker}");
            }
            assert_eq!(fs::read(site.join("assets").join(&svg))?, diagram);
            println!(
                "Generated private Reader with {} disclosures and {} KaTeX elements",
                html.matches("<details").count(),
                html.matches("class=\"katex\"").count()
            );
        }
        println!("{lang} Markdown {hash}; SVG {SVG}");
    }
    assert!(!root.join("website/private").exists());
    assert!(!root.join("website/public").exists());
    Ok(())
}
