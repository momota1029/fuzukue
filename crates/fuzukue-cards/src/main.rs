//! fuzukue-cards: カードハーネスの CLI。エージェントも人も同じものを使う。
//!
//!   fuzukue-cards [--root DIR] [--cards DIR]... [--rules DIR]... check [--only PREFIX]
//!   fuzukue-cards ... show <id>              カード本文と前後のリンク
//!   fuzukue-cards ... context <id>...        入口から読むべきカードを依存順に
//!   fuzukue-cards ... files <path>           そのファイルを指すカード
//!   fuzukue-cards ... graph                  JSON でグラフ全体

use std::path::PathBuf;

use anyhow::{bail, Result};
use fuzukue_cards::{Graph, Severity};

fn main() -> Result<()> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut root = PathBuf::from(".");
    let mut dirs = Vec::new();
    let mut rules = Vec::new();
    let mut only = None;
    while let Some(flag) = args.first().cloned() {
        match flag.as_str() {
            "--root" => { args.remove(0); root = PathBuf::from(args.remove(0)); }
            "--cards" => { args.remove(0); dirs.push(PathBuf::from(args.remove(0))); }
            "--rules" => {
                args.remove(0);
                if args.first().is_none_or(|arg| arg.is_empty() || arg.starts_with('-')) {
                    bail!("--rules requires a directory");
                }
                let path = PathBuf::from(args.remove(0));
                if !path.is_dir() {
                    bail!("{} is not a rule directory", path.display());
                }
                rules.push(path);
            }
            "--only" => {
                args.remove(0);
                if only.is_some() {
                    bail!("--only may be given once");
                }
                only = Some(code_prefix(&mut args)?);
            }
            _ => break,
        }
    }
    match args.first().map(String::as_str) {
        Some("prh") => {
            let dictionary = match args.as_slice() {
                [_, source] | [_, source, _] => fuzukue_cards::prh::Dictionary::read(&root.join(source))?,
                _ => bail!("fuzukue-cards prh <source.yml> [<output.yml>]"),
            };
            if let Some(output) = args.get(2) {
                dictionary.write(&root.join(output))?;
            }
            return Ok(());
        }
        Some("import-latex") => {
            let (main, deck, verify) = match args.as_slice() {
                [_, main, deck] => (main, deck, true),
                [_, main, deck, flag] if flag == "--no-verify" => (main, deck, false),
                _ => bail!("fuzukue-cards import-latex <main.tex> <out-deck> [--no-verify]"),
            };
            let result = fuzukue_cards::latex_book::import(&root.join(main), &root.join(deck), verify)?;
            println!("{} cards, {} ({})", result.cards, result.verification, result.report.display());
            return Ok(());
        }
        Some("build-latex") => {
            let [_, deck] = args.as_slice() else { bail!("fuzukue-cards build-latex <deck>") };
            println!("{}", fuzukue_cards::latex_book::build(&root.join(deck))?.display());
            return Ok(());
        }
        _ => {}
    }
    // --cards を指定しなければ、デッキの設定（cards.toml / harness.toml）に従う。
    let g = match (dirs.is_empty(), rules.is_empty()) {
        (true, true) => Graph::open(&root)?,
        (false, true) => Graph::load(&root, &dirs)?,
        (true, false) => Graph::open_with_rules(&root, &rules)?,
        (false, false) => Graph::load_with_rules(&root, &dirs, &rules)?,
    };
    let cmd = if args.is_empty() { "check".to_string() } else { args.remove(0) };
    if only.is_some() && cmd != "check" {
        bail!("--only is available for check");
    }
    match cmd.as_str() {
        "check" => {
            if args.first().is_some_and(|arg| arg == "--only") {
                args.remove(0);
                if only.is_some() {
                    bail!("--only may be given once");
                }
                only = Some(code_prefix(&mut args)?);
            }
            if let Some(arg) = args.first() {
                bail!("unknown check argument `{arg}`");
            }
            let diagnostics = g.diagnostics.iter().filter(|d| {
                only.as_ref().is_none_or(|prefix| d.code.is_some_and(|code| code.starts_with(prefix)))
            });
            let mut count = 0;
            let mut has_errors = false;
            for d in diagnostics {
                count += 1;
                has_errors |= d.severity == Severity::Error;
                let s = match d.severity { Severity::Error => "error", Severity::Warning => "warning" };
                if let Some(code) = d.code {
                    println!("{s}[{code}]: {}: {}", d.path.display(), d.display_message());
                } else {
                    println!("{s}: {}: {}", d.path.display(), d.display_message());
                }
            }
            println!("{} cards, {count} diagnostics", g.cards.len());
            if has_errors {
                std::process::exit(1);
            }
        }
        "show" => {
            let id = args.first().map(String::as_str).unwrap_or_default();
            let Some(c) = g.cards.get(id) else { bail!("no card `{id}`") };
            println!("# {} [{}] {}  ({})", c.id, c.kind, c.title, c.path.display());
            for l in &c.links { println!("→ {} {}", l.rel, l.to); }
            for (from, rel) in g.backlinks.get(id).into_iter().flatten() { println!("← {rel} {from}"); }
            for f in &c.files { println!("📄 {f}"); }
            println!("{}", c.body);
        }
        "context" => {
            for id in g.context(&args) {
                let c = &g.cards[&id];
                println!("{}\t{}\t{}", id, c.path.display(), c.title);
            }
        }
        "files" => {
            let p = args.first().map(String::as_str).unwrap_or_default();
            for id in g.cards_for_file(p) { println!("{id}\t{}", g.cards[&id].path.display()); }
        }
        "graph" => println!("{}", serde_json::to_string_pretty(&g.cards)?),
        other => bail!("unknown command `{other}`"),
    }
    Ok(())
}

fn code_prefix(args: &mut Vec<String>) -> Result<String> {
    if args.first().is_none_or(|arg| arg.is_empty() || arg.starts_with('-')) {
        bail!("--only requires a diagnostic code prefix");
    }
    Ok(args.remove(0))
}
