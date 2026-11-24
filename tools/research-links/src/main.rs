mod index;
mod scan;

use anyhow::{bail, Context, Result};
use index::{Index, Target};
use serde::Serialize;
use std::collections::{BTreeMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::{self, Write};
use std::path::Path;
use std::time::Instant;
use walkdir::WalkDir;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
struct Site {
    line: usize,
    column: usize, // UTF-8 byte column, 1-based
}

#[derive(Serialize)]
struct Symbol {
    kind: &'static str,
    name: String,
    site: Site,
    origin: &'static str,
}

#[derive(Serialize)]
struct Use {
    kind: &'static str,
    name: String,
    site: Site,
    target: Option<Target>,
}

#[derive(Serialize)]
struct Question {
    line: usize,
    end: usize,
    title: String,
}

#[derive(Serialize)]
struct Doc {
    id: usize,
    path: String,
    ext: String,
    folder: String,
    project: String,
    #[serde(skip)]
    lines: Vec<usize>,
    #[serde(skip)]
    fingerprint: u64,
    symbols: Vec<Symbol>,
    uses: Vec<Use>,
    questions: Vec<Question>,
}

#[derive(Default)]
struct ByteCount(usize);

impl Write for ByteCount {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 += bytes.len();
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn load(root: &Path) -> Result<(Vec<Doc>, Vec<String>, usize, usize)> {
    let entries = WalkDir::new(root)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|entry| {
            entry.depth() == 0
                || (!entry.file_name().to_string_lossy().starts_with('.')
                    && !(entry.file_type().is_dir()
                        && matches!(
                            entry.file_name().to_str(),
                            Some("target" | "build" | "node_modules" | "output")
                        )))
        });
    let mut docs = Vec::new();
    let mut bodies = Vec::new();
    let mut input_bytes = 0;
    let mut skipped = 0;
    for entry in entries {
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path().strip_prefix(root)?;
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        if !matches!(ext, "md" | "tex" | "lean" | "bib" | "txt") {
            continue;
        }
        let bytes =
            std::fs::read(entry.path()).with_context(|| format!("read {}", path.display()))?;
        let text = match std::str::from_utf8(&bytes) {
            Ok(text) => text,
            Err(_) => {
                eprintln!("Skipping non-UTF-8 source {}", path.display());
                skipped += 1;
                continue;
            }
        };
        input_bytes += bytes.len();
        let mut digest = DefaultHasher::new();
        bytes.hash(&mut digest);
        let folder = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .and_then(|_| path.components().next())
            .map_or_else(
                || ".".to_owned(),
                |c| c.as_os_str().to_string_lossy().into_owned(),
            );
        let parents = path.parent().unwrap_or(Path::new(""));
        let deck = parents.ancestors().find(|p| {
            root.join(p).join("deck.toml").is_file() || root.join(p).join("cards.toml").is_file()
        });
        let project = deck.map(|p| p.display().to_string()).unwrap_or_else(|| {
            parents
                .components()
                .take(2)
                .collect::<std::path::PathBuf>()
                .display()
                .to_string()
        });
        let mut doc = Doc {
            id: docs.len(),
            fingerprint: digest.finish(),
            path: path.display().to_string(),
            ext: ext.to_owned(),
            folder,
            project,
            lines: std::iter::once(0)
                .chain(text.match_indices('\n').map(|(i, _)| i + 1))
                .collect(),
            symbols: vec![],
            uses: vec![],
            questions: vec![],
        };
        bodies.push(scan::extract(&mut doc, text));
        docs.push(doc);
    }
    Ok((docs, bodies, input_bytes, skipped))
}

fn count<'a>(items: impl Iterator<Item = &'a str>) {
    let mut counts = BTreeMap::new();
    for item in items {
        *counts.entry(item).or_insert(0) += 1;
    }
    for (name, n) in counts {
        println!("  {name}: {n}");
    }
}

fn sample<T>(matches: &[T], limit: usize) -> impl Iterator<Item = &T> {
    let len = limit.min(matches.len());
    (0..len).map(move |i| &matches[i * matches.len() / len])
}

fn run() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.is_empty() || args[0] == "--help" {
        println!("research-links ROOT [--limit N] [--between PATH PATH] [--include-linked] [--include-duplicates] [--exclude PATH] [--find NAME | --audit ORIGIN | --questions FILTER]");
        return Ok(());
    }
    let root = Path::new(&args[0])
        .canonicalize()
        .context("research root")?;
    if !root.is_dir() {
        bail!("ROOT must be a directory");
    }
    let (mut limit, mut left, mut right, mut include_linked) = (30, "", "", false);
    let mut include_duplicates = false;
    let mut excluded = Vec::new();
    let (mut find, mut audit, mut questions) = (None, None, None);
    let mut args = args.iter().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--limit" => limit = args.next().context("--limit needs N")?.parse()?,
            "--between" => {
                left = args.next().context("--between needs two path substrings")?;
                right = args.next().context("--between needs two path substrings")?;
            }
            "--include-linked" => include_linked = true,
            "--include-duplicates" => include_duplicates = true,
            "--exclude" => excluded.push(
                args.next()
                    .context("--exclude needs a path substring")?
                    .as_str(),
            ),
            "--find" => find = Some(args.next().context("--find needs a symbol name")?),
            "--audit" => audit = Some(args.next().context("--audit needs an origin or kind")?),
            "--questions" => {
                questions = Some(
                    args.next()
                        .context("--questions needs a filter (use '' for all)")?,
                )
            }
            _ => bail!("unknown argument {arg}"),
        }
    }
    if [find, audit, questions].into_iter().flatten().count() > 1 {
        bail!("choose one output mode");
    }
    let start = Instant::now();
    let (docs, bodies, bytes, skipped) = load(&root)?;
    let index = Index::build(docs, &bodies)?;
    drop(bodies);
    let indexing = start.elapsed();
    let mut size = ByteCount::default();
    serde_json::to_writer(&mut size, &index.docs)?;
    let docs = &index.docs;
    println!("{} documents, {bytes} source bytes, {skipped} encoding skips, {:.3}s index, {} bytes compact JSON equivalent",
        docs.len(), indexing.as_secs_f64(), size.0);
    println!("Sources");
    count(docs.iter().map(|d| d.ext.as_str()));
    println!("Symbols");
    count(docs.iter().flat_map(|d| d.symbols.iter().map(|s| s.kind)));
    println!("References");
    count(docs.iter().flat_map(|d| d.uses.iter().map(|u| u.kind)));
    println!(
        "{} resolved reference occurrences; {} question markers",
        docs.iter()
            .flat_map(|d| &d.uses)
            .filter(|u| u.target.is_some())
            .count(),
        docs.iter().map(|d| d.questions.len()).sum::<usize>()
    );
    let ranking_start = Instant::now();
    if let Some(name) = find {
        index.find(name, limit);
    } else if let Some(kind) = audit {
        for kind in kind.split(',') {
            if let Some(kind) = kind.strip_prefix("use:") {
                let matches: Vec<_> = docs
                    .iter()
                    .flat_map(|d| d.uses.iter().map(move |u| (d, u)))
                    .filter(|(_, u)| u.kind == kind)
                    .collect();
                for (doc, u) in sample(&matches, limit) {
                    print!("{}:{} {} {}", doc.path, u.site.line, u.kind, u.name);
                    if let Some(Target(doc, symbol)) = u.target {
                        print!(
                            " -> {}:{}",
                            docs[doc].path,
                            docs[doc].symbols.get(symbol).map_or(1, |s| s.site.line)
                        );
                    }
                    println!();
                }
            } else {
                let matches: Vec<_> = docs
                    .iter()
                    .flat_map(|d| d.symbols.iter().map(move |s| (d, s)))
                    .filter(|(_, s)| s.kind == kind || s.origin == kind)
                    .collect();
                for (doc, s) in sample(&matches, limit) {
                    println!(
                        "{}:{} {} {} [{}]",
                        doc.path, s.site.line, s.kind, s.name, s.origin
                    );
                }
            }
        }
    } else if let Some(filter) = questions {
        let hubs = index.hubs(&filter.to_lowercase());
        println!("{} question markers with cross-folder matches", hubs.len());
        let mut projects = HashSet::new();
        for (_, id, q, related) in hubs
            .into_iter()
            .filter(|(_, id, _, _)| projects.insert(&docs[*id].project))
            .take(limit)
        {
            let question = &docs[id].questions[q];
            println!("{}:{} {}", docs[id].path, question.line, question.title);
            for (other, term, site) in related {
                println!("    {}:{} [{term}]", docs[other].path, site.line);
            }
        }
    } else {
        let pairs = index.results(left, right, &excluded, include_linked, include_duplicates);
        println!(
            "{} candidate document pairs before diversification",
            pairs.len()
        );
        let mut seen = BTreeMap::new();
        for (i, ((a, b), pair)) in pairs
            .into_iter()
            .filter(|((a, b), _)| {
                let group = seen
                    .entry((&docs[*a].project, &docs[*b].project))
                    .or_insert(0);
                *group += 1;
                *group <= 2
            })
            .take(limit)
            .enumerate()
        {
            let [p, q] = pair.terms[0].2;
            println!(
                "{:>2}. {:.2}{} {}:{} <> {}:{}",
                i + 1,
                pair.score,
                if index.linked(a, b) { " [linked]" } else { "" },
                docs[a].path,
                p.line,
                docs[b].path,
                q.line
            );
            println!(
                "    {}",
                pair.terms
                    .iter()
                    .take(5)
                    .map(|(_, term, _)| term.as_str())
                    .collect::<Vec<_>>()
                    .join(" | ")
            );
        }
    }
    println!(
        "{:.3}s output query, {:.3}s total",
        ranking_start.elapsed().as_secs_f64(),
        start.elapsed().as_secs_f64()
    );
    Ok(())
}

fn main() -> Result<()> {
    run()
}
