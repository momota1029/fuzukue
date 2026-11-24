use std::path::{Path, PathBuf};
use std::process::Command;

use fuzukue_cards::{find_decks, Graph, Severity};

fn example_decks() -> Vec<PathBuf> {
    let examples = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples");
    let mut decks = find_decks(&examples.join("lab"));
    assert!(!decks.is_empty());
    decks.push(examples.join("code-pilot"));
    decks
}

#[test]
fn example_decks_have_no_style_errors() {
    for deck in example_decks() {
        let graph = Graph::open(&deck).unwrap();
        assert!(!graph.cards.is_empty(), "{}", deck.display());
        let style: Vec<_> = graph
            .diagnostics
            .iter()
            .filter(|d| d.code.is_some_and(|code| code.starts_with("style/")))
            .collect();
        assert!(
            style.iter().all(|d| d.severity == Severity::Warning),
            "{}: {style:?}",
            deck.display()
        );
    }
}

#[test]
fn check_filters_diagnostic_codes_and_summary() {
    struct Fixture(PathBuf);
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let dir = std::env::temp_dir().join(format!("fuzukue-style-cli-{}", std::process::id()));
    std::fs::create_dir(&dir).unwrap();
    let deck = Fixture(dir);
    std::fs::create_dir(deck.0.join("cards")).unwrap();
    std::fs::write(
        deck.0.join("cards/check.md"),
        "---\nid: check\nrequires: [missing]\n---\nA crucial result might possibly hold.\n",
    )
    .unwrap();

    let check = |options: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_fuzukue-cards"))
            .arg("--root")
            .arg(&deck.0)
            .args(options)
            .output()
            .unwrap()
    };
    let unfiltered = check(&["check"]);
    assert_eq!(unfiltered.status.code(), Some(1));
    let all = String::from_utf8(unfiltered.stdout).unwrap();
    assert!(all.contains("error: cards/check.md"));

    for options in [["check", "--only", "style"], ["--only", "style", "check"]] {
        let output = check(&options);
        assert!(output.status.success());
        let stdout = String::from_utf8(output.stdout).unwrap();
        let lines: Vec<_> = stdout.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].starts_with("warning[style/ai-phrase]: cards/check.md: "));
        assert!(lines[1].starts_with("warning[style/hedging]: cards/check.md: "));
        assert_eq!(lines[2], "1 cards, 2 diagnostics");
    }

    let output = check(&["check", "--only", "style/hedg"]);
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.starts_with("warning[style/hedging]:"));
    assert!(stdout.ends_with("1 cards, 1 diagnostics\n"));
}

#[cfg(unix)]
#[test]
fn external_textlint_merges_positions_and_reports_unavailable() -> anyhow::Result<()> {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    let Some(project) = fuzukue_proto::compat::env_os("FUZUKUE_TEST_TEXTLINT_PROJECT").map(PathBuf::from) else {
        let dir = tempfile::tempdir()?;
        let path =
            std::env::join_paths([dir.path().join("bin"), "/usr/bin".into(), "/bin".into()])?;
        let output = Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "external_textlint_merges_positions_and_reports_unavailable",
                "--nocapture",
            ])
            .env("FUZUKUE_TEST_TEXTLINT_PROJECT", dir.path())
            .env("PATH", path)
            .output()?;
        anyhow::ensure!(
            output.status.success(),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return Ok(());
    };
    let deck = project.join("paper");
    let bin = project.join("bin");
    fs::create_dir_all(deck.join("cards"))?;
    fs::create_dir(&bin)?;
    fs::write(project.join("fuzukue.toml"), "[style]\ntextlint = 'auto'\n")?;
    fs::write(project.join(".textlintrc.yml"), "{}\n")?;
    fs::write(
        deck.join("cards/check.md"),
        r#"---
id: check
lint_ignore: [skip-this]
---
```text
隠し文字
```
\cite{隠し文献
追加}
資料を参照する．
😀 `隠しコード`語を読む。
"#,
    )?;
    let fake = bin.join("textlint");
    fs::write(
        &fake,
        r#"#!/bin/sh
[ "$1 $2 $3" = "--format json --config" ] || exit 2
[ -f "$4" ] || exit 2
# Check that neither TeX nor Markdown code reaches textlint.
awk '/資料を参照する/{a=NR} /語を読む/{b=NR} /隠し|追加/{invalid=1}
END {
    if (!a || !b || invalid) exit 2
    printf "[{\"filePath\":\"%s\",\"messages\":[", FILENAME
    printf "{\"ruleId\":\"preset-ja-technical-writing/ja-no-mixed-period\",\"line\":%d,\"column\":8,\"message\":\"duplicate native\"},",a
    printf "{\"ruleId\":\"external-only\",\"line\":%d,\"column\":7,\"message\":\"external message\"},",b
    printf "{\"ruleId\":\"external-only\",\"line\":%d,\"column\":7,\"message\":\"duplicate external\"},",b
    printf "{\"ruleId\":\"skip-this\",\"line\":%d,\"column\":7,\"message\":\"ignored\"}]}]\n",b
}' "$5" || exit 2
exit 1
"#,
    )?;
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o755))?;
    let graph = Graph::open(&deck)?;
    let diags = &graph.diagnostics;
    assert_eq!(diags.len(), 2, "{diags:?}");
    assert_eq!(
        diags[0].textlint_rule.as_deref(),
        Some("ja-no-mixed-period")
    );
    assert_eq!(diags[0].line, Some(6));
    assert_eq!(diags[1].textlint_rule.as_deref(), Some("external-only"));
    assert_eq!(diags[1].line, Some(7));
    assert_eq!(diags[1].column, Some(11)); // UTF-16, including the removed inline code.
    assert!(diags[1].fix_hint.is_some());

    fs::rename(&fake, bin.join("checker"))?;
    let npx = bin.join("npx");
    fs::write(
        &npx,
        r#"#!/bin/sh
[ "$1 $2 $3 $4" = "--offline --no-install -- textlint" ] || exit 2
[ "$npm_config_offline $npm_config_yes" = "true false" ] || exit 2
shift 4
exec ./bin/checker "$@"
"#,
    )?;
    fs::set_permissions(npx, fs::Permissions::from_mode(0o755))?;
    assert_eq!(
        Graph::open(&deck)?.diagnostics[1].textlint_rule.as_deref(),
        Some("external-only")
    );

    fs::write(project.join("fuzukue.toml"), "[style]\ntextlint = 'off'\n")?;
    assert_eq!(Graph::open(&deck)?.diagnostics.len(), 1);
    fs::write(
        project.join("fuzukue.toml"),
        "[style]\ntextlint = './bin/does-not-exist'\n",
    )?;
    let graph = Graph::open(&deck)?;
    assert!(graph
        .diagnostics
        .iter()
        .any(|d| d.code == Some("style/textlint-status")));
    Ok(())
}
