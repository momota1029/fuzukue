use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use anyhow::Result;
use serde_json::Value;
use tempfile::TempDir;

fn run(root: &Path, args: &[&str], success: bool) -> Output {
    let output = Command::new(env!("CARGO_BIN_EXE_reading-companion"))
        .current_dir(root)
        .args(args)
        .output()
        .unwrap();
    assert_eq!(
        output.status.success(),
        success,
        "{args:?}\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn read_json(path: &Path) -> Result<Value> {
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}

fn mutate(path: &Path, update: impl FnOnce(&mut Value)) -> Result<()> {
    let mut json = read_json(path)?;
    update(&mut json);
    fs::write(path, serde_json::to_vec_pretty(&json)?)?;
    Ok(())
}

fn review_fixture() -> Result<TempDir> {
    use serde_json::json;
    let dir = tempfile::tempdir()?;
    let root = dir.path();
    let mut step = json!({
        "id": "S1", "title": "The key step", "statement": "Show the bound",
        "deps": [], "loc": "§1", "origin": "new", "tier": "key",
        "why_tier": "The target uses this bound", "role": "bound"
    });
    let first = step.clone();
    step["id"] = json!("T");
    step["title"] = json!("Target");
    step["deps"] = json!(["S1"]);
    step["tier"] = json!("semi");
    fs::write(
        root.join("steps.json"),
        json!({"steps": [first, step]}).to_string(),
    )?;
    fs::write(
        root.join("bodies.json"),
        json!({
            "S1": {
                "l1": "<p>Compute <span class=\"m\">x^2</span>.</p>",
                "l2": "<p>Key reasoning.</p>",
                "l3": "<div class=\"d\">x^2 \u{003e} 0</div>"
            }
        })
        .to_string(),
    )?;
    let axes: Vec<_> = [
        "correctness",
        "novelty",
        "delta",
        "strength",
        "significance",
        "literature",
    ]
    .into_iter()
    .map(|key| json!({"key":key, "level":"mid", "why":"Synthetic evidence"}))
    .collect();
    fs::write(root.join("review.json"), json!({
        "title": "Local <review>",
        "summary_html": "<p>Synthetic review</p>",
        "panel": [],
        "review_conclusion": "No gap found within the stated scope",
        "findings": [{
            "id":"F1","sev":"minor","status":"suspected","steps":["S1"],
            "from":["line-by-line"],"short":"State a hypothesis","detail":"<p>Explain the hypothesis.</p>"
        }],
        "prior": [],
        "indicator": {
            "facts":{"review_done":true, "delta_stated":true},
            "verdict":"short_or_preprint", "verdict_why":"Synthetic comparison.",
            "axes": axes, "changers":["Check one example."]
        }
    }).to_string())?;
    Ok(dir)
}

#[test]
fn review_routes_share_the_ledger_and_verdict_but_standalone_keeps_demo_and_links() -> Result<()> {
    use fuzukue_reading::review::{deck, Language};
    let dir = review_fixture()?;
    let root = dir.path();
    fs::write(
        root.join("demo.html"),
        r#"<div id="play">Try it</div><script>document.getElementById('play');</script>"#,
    )?;
    let katex = root.join("katex");
    fs::create_dir_all(katex.join("fonts"))?;
    fs::write(katex.join("katex.min.js"), "/* test KaTeX asset */")?;
    fs::write(
        katex.join("katex.min.css"),
        "@font-face{src:url(fonts/Test.woff2)}",
    )?;
    fs::write(katex.join("fonts/Test.woff2"), "font")?;
    for lang in ["en", "ja"] {
        run(
            root,
            &[
                "build_review_page",
                ".",
                "--out",
                "review.html",
                "--lang",
                lang,
                "--katex-dir",
                "katex",
            ],
            true,
        );
        let page = fs::read_to_string(root.join("review.html"))?;
        for marker in [
            r#"<details class="step t-keyc" id="step-S1" open>"#,
            r#"<details class="l2" open>"#,
            r#"<details class="l3">"#,
            r##"<a href="#step-S1"><rect"##,
            r##"href="#find-F1""##,
            "Local &lt;review&gt;",
            r#"<span class="m">x^2</span>"#,
            r#"id="play""#,
            "data:font/woff2;base64,Zm9udA==",
        ] {
            assert!(page.contains(marker), "{marker}");
        }
        assert!(page.contains(&format!(r#"<html lang="{lang}">"#)));
        assert!(
            page.rfind("<script>document.getElementById('play');</script>")
                .unwrap()
                > page.find("</main>").unwrap()
        );
        run(
            root,
            &["check_review_page", "review.html", "--spec", "."],
            true,
        );
        let site = deck(root, "local-review", Language::parse(lang)?)?;
        assert!(site.markdown.contains("::: details"));
        assert!(site.markdown.contains("asset:diagram.svg"));
        assert!(!site.markdown.contains("id=\"play\""));
        assert!(site.diagram.contains("href=\"#step-S1\""));
    }
    let clean = fs::read_to_string(root.join("review.html"))?;
    fs::write(
        root.join("broken.html"),
        clean.replace(r#"id="step-S1""#, r#"id="gone""#),
    )?;
    let broken = run(
        root,
        &["check_review_page", "broken.html", "--spec", "."],
        false,
    );
    assert!(stdout(&broken).contains("link #step-S1 has no target"));
    assert!(stdout(&broken).contains("no card with id step-S1"));

    mutate(&root.join("review.json"), |data| {
        data["indicator"]["verdict"] = "journal".into()
    })?;
    assert!(stderr(&run(root, &["indicator", "review.json"], false)).contains("exceeds"));
    run(
        root,
        &["build_review_page", ".", "--out", "review.html"],
        false,
    );
    assert_eq!(fs::read_to_string(root.join("review.html"))?, clean);
    assert!(deck(root, "local-review", Language::En).is_err());
    mutate(&root.join("review.json"), |data| {
        data["indicator"]["verdict"] = "short_or_preprint".into();
        data["findings"][0]["steps"][0] = "unknown".into();
    })?;
    let result = run(
        root,
        &["build_review_page", ".", "--out", "review.html"],
        false,
    );
    assert!(stderr(&result).contains("finding F1 refers to unknown step"));
    Ok(())
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn indicator_enforces_each_evidence_cap() -> Result<()> {
    use serde_json::json;
    let dir = review_fixture()?;
    let root = dir.path();
    let path = root.join("review.json");
    let base = read_json(&path)?;
    run(root, &["indicator", "review.json"], true);
    for (facts, axis, expected) in [
        (json!({"review_done": false}), None, "review is not done"),
        (json!({"fatal_confirmed": 1}), None, "'needs_work'"),
        (json!({}), Some(0), "expert check or formal proof"),
        (
            json!({"closest_prior_unread": true}),
            Some(1),
            "closest prior work is unread",
        ),
        (json!({}), Some(5), "limited source list"),
        (json!({"delta_stated": false}), None, "'needs_work'"),
    ] {
        let mut spec = base.clone();
        spec["indicator"]["facts"]
            .as_object_mut()
            .unwrap()
            .extend(facts.as_object().unwrap().clone());
        if let Some(axis) = axis {
            spec["indicator"]["axes"][axis]["level"] = "strong".into();
        }
        fs::write(&path, spec.to_string())?;
        assert!(stderr(&run(root, &["indicator", "review.json"], false)).contains(expected));
    }
    fs::write(&path, base.to_string())?;
    mutate(&path, |spec| {
        spec["indicator"]["facts"]["attack_survived"] = true.into();
        spec["indicator"]["axes"][1]["level"] = "strong".into();
        spec["indicator"]["verdict"] = "journal".into();
    })?;
    run(root, &["indicator", "review.json"], true);
    mutate(&path, |spec| spec["indicator"]["changers"] = json!([]))?;
    assert!(stderr(&run(root, &["indicator", "review.json"], false)).contains("changers"));
    let mut rule_input = base;
    let indicator = rule_input["indicator"].as_object_mut().unwrap();
    indicator.remove("verdict_why");
    for axis in indicator["axes"].as_array_mut().unwrap() {
        axis.as_object_mut().unwrap().remove("why");
    }
    fs::write(path, rule_input.to_string())?;
    run(root, &["indicator", "review.json"], true);
    run(
        root,
        &["build_review_page", ".", "--out", "review.html"],
        false,
    );
    Ok(())
}

#[test]
fn prose_builds_reader_markdown_with_nested_folds_and_embedded_assets() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path();
    fs::create_dir_all(root.join("figures/prose"))?;
    fs::write(
        root.join("figures/prose/example.svg"),
        r#"<svg xmlns="http://www.w3.org/2000/svg"></svg>"#,
    )?;
    let head = "+++\ntitle = 'A short example'\nslug = 'prose'\nlanguage = 'ja'\n+++\n";
    let body = r##"
## First

A <script>fake()</script> literal, $x<y$, and [the note](#inner).

| A | B |
|---|---|
| a | b |

![A figure](asset:example.svg)

::: details Outer $x^2$ {#outer}

$$
x^2 + y^2
$$

::: details Inner {#inner}

```text
::: details This is code
```

:::

:::

## Second
"##;
    fs::write(root.join("page.md"), format!("{head}{body}"))?;
    run(
        root,
        &["build_prose_page", "page.md", "--out", "page.html"],
        true,
    );
    let page = fs::read_to_string(root.join("page.html"))?;
    for marker in [
        r#"<html lang="ja">"#,
        r#"id="section-2""#,
        r#"id="outer""#,
        r#"id="inner""#,
        "<figure><svg",
        "<table>",
        r#"<div class="d">x^2 + y^2</div>"#,
        r#"<span class="m">x&lt;y</span>"#,
        "&lt;script&gt;fake()&lt;/script&gt;",
        "::: details This is code",
    ] {
        assert!(page.contains(marker), "{marker}");
    }
    run(root, &["check_review_page", "page.html"], true);
    fs::write(
        root.join("page-en.md"),
        format!(
            "{}{body}",
            head.replace("slug = 'prose'", "slug = 'prose-en'")
        ),
    )?;
    fs::write(
        root.join("template.html"),
        "<html lang=\"{{LANG}}\"><head><title>{{TITLE}}</title></head><main><p>{{L:ax_novelty}}</p>{{BODY}}</main></html>",
    )?;
    run(
        root,
        &[
            "build_prose_page",
            "page-en.md",
            "--out",
            "page-en.html",
            "--lang",
            "en",
            "--template",
            "template.html",
        ],
        true,
    );
    let english = fs::read_to_string(root.join("page-en.html"))?;
    assert!(english.contains(r#"<html lang="en">"#));
    assert!(english.contains("<p>Novelty of the key part</p>"));
    assert!(english.contains("<figure><svg"));
    for body in [
        "::: details open\n",
        "::: details reserved {#section-1}\n\n:::\n",
        "::: widget unsupported\n",
        "![svg](asset:missing.svg)\n",
        "[bad](javascript:alert%281%29)\n",
        "$$\nx+y\n",
    ] {
        fs::write(root.join("page.md"), format!("{head}\n{body}"))?;
        run(
            root,
            &["build_prose_page", "page.md", "--out", "page.html"],
            false,
        );
        assert_eq!(fs::read_to_string(root.join("page.html"))?, page);
    }
    Ok(())
}
