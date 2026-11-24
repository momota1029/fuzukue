//! Private reading artifacts from the reading-proof-review JSON and derivation formats.

mod archive;
mod companion;
mod language;
mod proof;
mod section;
mod support;

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{bail, ensure, Context, Result};
use fuzukue_reading::{html, prose, review, review::Language};

const USAGE: &str = "reading-companion build_companion SPEC --out HTML [--template HTML]
reading-companion check_companion SPEC HTML [--workspace ROOT] [--max-kb N] [--no-dom] [--allow-pending]
reading-companion check_outline SPEC
reading-companion build_section_page RECORD --out DIR [--base DIR] [--original PDF]
reading-companion check_section_record RECORD
reading-companion proof_book build|check|refs PROJECT [--block ID] [--builder COMMAND]
reading-companion build_review_page SPEC_DIR --out HTML [--lang en|ja] [--katex-dir DIR] [--template HTML]
reading-companion build_prose_page PAGE.md --out HTML [--lang en|ja] [--katex-dir DIR] [--template HTML]
reading-companion check_review_page HTML [--spec SPEC_DIR] [--katex-dir DIR]
reading-companion indicator REVIEW.json
reading-companion archive_pair PAIR --workspace ROOT";

fn main() -> std::process::ExitCode {
    match run(std::env::args().skip(1)) {
        Ok(true) => std::process::ExitCode::SUCCESS,
        Ok(false) => std::process::ExitCode::FAILURE,
        Err(error) => {
            eprintln!("{error:#}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn run(mut args: impl Iterator<Item = String>) -> Result<bool> {
    let command = args.next().unwrap_or_else(|| "--help".into());
    let command = command.replace('-', "_");
    let (options, switches, count): (&[&str], &[&str], _) = match command.as_str() {
        "build_companion" => (&["out", "template"], &[], 1),
        "check_companion" => (&["workspace", "max-kb"], &["no-dom", "allow-pending"], 2),
        "check_outline" | "check_section_record" | "indicator" => (&[], &[], 1),
        "build_section_page" => (&["out", "base", "original"], &[], 1),
        "proof_book" => (&["block", "builder"], &[], 2),
        "build_review_page" | "build_prose_page" => {
            (&["out", "lang", "katex-dir", "template"], &[], 1)
        }
        "check_review_page" => (&["spec", "katex-dir"], &[], 1),
        "archive_pair" => (&["workspace"], &[], 1),
        "__help" | "_h" => {
            println!("{USAGE}");
            return Ok(true);
        }
        _ => bail!("unknown command {command:?}\n{USAGE}"),
    };

    let mut positions = Vec::new();
    let mut values = BTreeMap::new();
    let mut flags = BTreeSet::new();
    while let Some(argument) = args.next() {
        if argument == "--help" || argument == "-h" {
            println!("{USAGE}");
            return Ok(true);
        }
        if argument == "--" {
            positions.extend(args);
            break;
        }
        if let Some(flag) = argument.strip_prefix("--") {
            if switches.contains(&flag) {
                ensure!(flags.insert(flag.to_owned()), "duplicate option --{flag}");
                continue;
            }
            let (key, inline) = flag
                .split_once('=')
                .map_or((flag, None), |(k, v)| (k, Some(v)));
            ensure!(options.contains(&key), "unknown option --{key}");
            let value = inline
                .map(str::to_owned)
                .or_else(|| args.next())
                .with_context(|| format!("--{key} needs a value"))?;
            ensure!(
                values.insert(key.to_owned(), value).is_none(),
                "duplicate option --{key}"
            );
        } else {
            positions.push(argument);
        }
    }
    ensure!(positions.len() == count, "{USAGE}");
    let input = Path::new(&positions[0]);
    let option = |key: &str| values.get(key).map(String::as_str);
    let path = |key: &str| option(key).map(Path::new);
    let required = |key: &str| path(key).with_context(|| format!("--{key} is required"));

    match command.as_str() {
        "build_companion" => companion::build(input, required("out")?, path("template")),
        "check_companion" => companion::check(
            input,
            Path::new(&positions[1]),
            path("workspace"),
            option("max-kb").unwrap_or("8000").parse()?,
            flags.contains("no-dom"),
            flags.contains("allow-pending"),
        ),
        "check_outline" => companion::outline(input),
        "build_section_page" => {
            section::build(input, required("out")?, path("base"), path("original"))
        }
        "check_section_record" => section::check(input),
        "indicator" => {
            review::check_indicator(input)?;
            println!("indicator consistent with the rules");
            Ok(true)
        }
        "build_review_page" | "build_prose_page" => {
            let output = required("out")?;
            let lang = option("lang").map(Language::parse).transpose()?;
            let katex = path("katex-dir");
            let template = path("template");
            let html = if command == "build_review_page" {
                review::standalone(input, lang.unwrap_or(Language::En), katex, template)?
            } else {
                prose::standalone(input, lang, katex, template)?
            };
            support::write(output, html)?;
            println!("wrote {}", output.display());
            Ok(true)
        }
        "check_review_page" => {
            let check = html::check(input, path("spec"), path("katex-dir"))?;
            for problem in &check.problems {
                println!("ERROR {problem}");
            }
            if !check.math_checked {
                println!(
                    "NOTE math/script syntax check skipped (needs Node and a KaTeX directory)"
                );
            }
            if check.problems.is_empty() {
                println!("page structure OK");
            }
            Ok(check.problems.is_empty())
        }
        "archive_pair" => {
            archive::archive(input, required("workspace")?)?;
            Ok(true)
        }
        "proof_book" => {
            let project = Path::new(&positions[1]);
            match positions[0].as_str() {
                "build" => proof::build(project, option("builder")),
                "check" => proof::check(project),
                "refs" => proof::refs(
                    project,
                    option("block").context("--block is required for refs")?,
                ),
                cmd => bail!("unknown proof_book command {cmd:?}"),
            }
        }
        _ => unreachable!(),
    }
}
