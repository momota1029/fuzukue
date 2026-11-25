//! デッキから PDF を組み立てる。カードの TeX を \input して SyncTeX の逆引きを保つ。

use std::ffi::OsString;
use std::fmt::{self, Write};
use std::path::{Component, Path, PathBuf};

use anyhow::{bail, ensure, Context, Result};
use fuzukue_cards::latex_book::Engine;
use fuzukue_cards::{AspectRatio, Card, DeckConfig, DeckMeta, Graph};
use serde::Deserialize;
use serde_yaml::Value;
use tokio::process::Command;

use crate::cancellation::{Cancellation, Cancelled};

#[path = "harness_build.rs"]
pub(crate) mod harness;
#[path = "paper_batch.rs"]
pub(crate) mod batch;

const HEADER: &str =
    "% fuzukue が structure.yaml から作った。手で編集しない（カードの .tex を直す）。\n";
const ARTICLE: &str = "\\documentclass[uplatex,dvipdfmx,11pt]{jsarticle}\n";
const MATH: &str = "\\usepackage{amsmath,amssymb,amsthm,mathtools}\n\\usepackage{enumitem}\n";
const LINKS: &str = "\\usepackage[dvipdfmx,hidelinks]{hyperref}\n";
const CLEVEREF: &str = "\\usepackage[capitalise,nameinlink]{cleveref}\n";
const CREF_NAMES: &str = concat!(
    "\\crefname{theorem}{定理}{定理}\\crefname{lemma}{補題}{補題}\\crefname{proposition}{命題}{命題}\n",
    "\\crefname{corollary}{系}{系}\\crefname{definition}{定義}{定義}\\crefname{example}{例}{例}\n"
);
const BODY_MARKER: &str = "%%LABO-BODY%%";
const PREAMBLE_MARKER: &str = "%%LABO-PREAMBLE%%";

fn env_of(kind: &str) -> Option<&str> {
    match kind {
        "definition" | "theorem" | "lemma" | "proposition" | "corollary" | "example" | "remark" => {
            Some(kind)
        }
        _ => None,
    }
}

fn macros(meta: &DeckMeta) -> String {
    let mut tex = String::new();
    for (name, body) in &meta.macros {
        writeln!(tex, "\\newcommand{{\\{name}}}{{{body}}}").unwrap();
    }
    for (name, (body, n)) in &meta.macros_with_args {
        writeln!(tex, "\\newcommand{{\\{name}}}[{n}]{{{body}}}").unwrap();
    }
    tex
}

fn theorems(number_within: &str) -> String {
    format!(
        "\\theoremstyle{{plain}}\n\\newtheorem{{theorem}}{{定理}}{number_within}\n{}",
        concat!(
            "\\newtheorem{lemma}[theorem]{補題}\n",
            "\\newtheorem{proposition}[theorem]{命題}\n\\newtheorem{corollary}[theorem]{系}\n",
            "\\theoremstyle{definition}\n\\newtheorem{definition}[theorem]{定義}\n",
            "\\newtheorem{example}[theorem]{例}\n",
            "\\theoremstyle{remark}\n\\newtheorem{remark}[theorem]{注意}\n"
        )
    )
}

fn input(path: &Path) -> String {
    format!(
        "\\input{{../{}}}\n",
        path.to_string_lossy().replace('\\', "/")
    )
}

fn card_input(card: &Card, stem: &str) -> Option<String> {
    let file = card
        .tex
        .iter()
        .find(|p| p.file_stem().is_some_and(|s| s == stem))?;
    // card.path は外部デッキへの相対パスも含む。.tex は card.md と同じフォルダにある。
    Some(input(&card.path.with_file_name(file.file_name()?)))
}

fn statement(card: &Card, show_title: bool) -> String {
    let id = &card.id;
    let Some(source) = card_input(card, "statement") else {
        return format!("% カード {id} に statement.tex がない\n");
    };
    let label = format!("\\label{{card:{id}}}\n");
    if let Some(env) = env_of(&card.kind) {
        let title = if show_title && !card.title.is_empty() {
            format!("[{}]", card.title)
        } else {
            String::new()
        };
        format!("\\begin{{{env}}}{title}{label}{source}\\end{{{env}}}\n")
    } else {
        label + &source
    }
}

fn section_body(graph: &Graph, structure: &Value, heading: &str, subheading: &str) -> String {
    let mut tex = String::new();
    for sec in structure["sections"].as_sequence().into_iter().flatten() {
        let title = sec["title"].as_str().unwrap_or("");
        writeln!(tex, "\n{heading}{{{title}}}").unwrap();
        for item in sec["items"].as_sequence().into_iter().flatten() {
            if let Some(sub) = item["subsection"].as_str() {
                writeln!(tex, "{subheading}{{{sub}}}").unwrap();
                continue;
            }
            let Some(id) = item.as_str() else { continue };
            let Some(card) = graph.cards.get(id) else {
                writeln!(tex, "% カード {id} がない").unwrap();
                continue;
            };
            writeln!(tex, "% card {id}").unwrap();
            tex.push_str(&statement(card, true));
            if let Some(proof) = card_input(card, "proof") {
                writeln!(tex, "\\begin{{proof}}\n{proof}\\end{{proof}}").unwrap();
            }
            tex.push_str("% end card\n");
        }
    }
    tex
}

fn paper_tex(meta: &DeckMeta, graph: &Graph, structure: &Value) -> String {
    let title = structure["title"].as_str().unwrap_or(&meta.title);
    let macros = macros(meta);
    let theorems = theorems("[section]");
    let body = section_body(graph, structure, "\\section", "\\subsection");
    format!(
        "{HEADER}{ARTICLE}{MATH}{LINKS}{CLEVEREF}{macros}{theorems}{CREF_NAMES}\\title{{{title}}}\n         \\date{{}}\n\\begin{{document}}\n\\maketitle\n{body}\\end{{document}}\n"
    )
}

#[derive(Deserialize)]
struct Slides {
    #[serde(default)]
    sections: Vec<SlideSection>,
}

#[derive(Deserialize)]
struct SlideSection {
    title: String,
    #[serde(alias = "items")]
    frames: Vec<Frame>,
}

#[derive(Deserialize)]
#[serde(untagged, deny_unknown_fields)]
enum Frame {
    Card(String),
    Group { frame: String, cards: Vec<String> },
    Tex { frame: String, tex: String },
}

fn aspect_option(ratio: &AspectRatio) -> Result<String> {
    let ratio = match ratio {
        AspectRatio::Code(code) => code.to_string(),
        AspectRatio::Text(text) => text.replace(':', ""),
    };
    ensure!(
        matches!(
            ratio.as_str(),
            "2013" | "1610" | "169" | "149" | "141" | "54" | "43" | "32"
        ),
        "Beamer のアスペクト比に対応していない: {ratio}"
    );
    Ok(format!(",aspectratio={ratio}"))
}

fn slide_card<'a>(graph: &'a Graph, id: &str) -> Result<&'a Card> {
    graph
        .cards
        .get(id)
        .with_context(|| format!("カード {id} がない"))
}

fn slide_card_body(tex: &mut String, card: &Card, show_title: bool) {
    writeln!(tex, "% card {}", card.id).unwrap();
    if let Some(source) = card_input(card, "slide") {
        write!(tex, "\\label{{card:{}}}\n{source}", card.id).unwrap();
    } else {
        tex.push_str(&statement(card, show_title));
    }
    tex.push_str("% end card\n");
}

fn slides_tex(deck: &Path, meta: &DeckMeta, graph: &Graph, structure: Value) -> Result<String> {
    let title = structure["title"].as_str().unwrap_or(&meta.title);
    let options = meta.slides.as_ref();
    let ratio = options
        .and_then(|opts| opts.aspect_ratio.as_ref())
        .map(aspect_option)
        .transpose()?
        .unwrap_or_default();
    let mut tex = format!(
        "{HEADER}\\documentclass[dvipdfmx,uplatex,notheorems{ratio}]{{beamer}}\n         \\usepackage{{bxdpx-beamer,pxjahyper,mathtools}}\n"
    );
    if let Some(theme) = options.and_then(|opts| opts.theme.as_deref()) {
        writeln!(tex, "\\usetheme{{{theme}}}").unwrap();
    }
    tex.push_str(&macros(meta));
    tex.push_str(&theorems(""));
    let title = options
        .and_then(|opts| opts.title.as_deref())
        .unwrap_or(title);
    let author = options
        .and_then(|opts| opts.author.as_deref())
        .or(meta.author.as_deref())
        .unwrap_or("");
    let date = options
        .and_then(|opts| opts.date.as_deref())
        .or(meta.date.as_deref())
        .unwrap_or("");
    writeln!(
        tex,
        "\\title{{{title}}}\n\\author{{{author}}}\n\\date{{{date}}}"
    )
    .unwrap();
    tex.push_str("\\begin{document}\n\\begin{frame}\\titlepage\\end{frame}\n");

    let slides: Slides =
        serde_yaml::from_value(structure).context("スライドの structure.yaml を読めない")?;
    for sec in slides.sections {
        writeln!(tex, "\n\\section{{{}}}", sec.title).unwrap();
        for frame in sec.frames {
            match frame {
                Frame::Card(id) => {
                    let card = slide_card(graph, &id)?;
                    writeln!(tex, "\\begin{{frame}}{{{}}}", card.title).unwrap();
                    slide_card_body(&mut tex, card, false);
                }
                Frame::Group { frame, cards } => {
                    writeln!(tex, "\\begin{{frame}}{{{frame}}}").unwrap();
                    for id in cards {
                        slide_card_body(&mut tex, slide_card(graph, &id)?, true);
                    }
                }
                Frame::Tex { frame, tex: file } => {
                    let path = relative_file(deck, &file)?;
                    writeln!(tex, "\\begin{{frame}}{{{frame}}}").unwrap();
                    tex.push_str(&input(path));
                }
            }
            tex.push_str("\\end{frame}\n");
        }
    }
    tex.push_str("\\end{document}\n");
    Ok(tex)
}

fn relative_file<'a>(root: &Path, file: &'a str) -> Result<&'a Path> {
    let path = Path::new(file);
    ensure!(path.is_relative(), "相対パスで指定する: {file}");
    let full = crate::safe_join(root, file).with_context(|| format!("{file} を開けない"))?;
    ensure!(full.is_file(), "{file} はファイルではない");
    Ok(path)
}

fn document_tex(
    project: &Path,
    meta: &DeckMeta,
    graph: &Graph,
    structure: &Value,
) -> Result<String> {
    let template = meta.template.as_deref().unwrap_or("article");
    let title = structure["title"].as_str().unwrap_or(&meta.title);
    let author = meta.author.as_deref().unwrap_or("");
    let date = meta.date.as_deref().unwrap_or("");
    let macros = macros(meta);
    let (class, heading, subheading, counter) = match template {
        "article" => (ARTICLE, "\\section", "\\subsection", "[section]"),
        "report" => (
            "\\documentclass[uplatex,dvipdfmx,11pt]{jsreport}\n",
            "\\chapter",
            "\\section",
            "[chapter]",
        ),
        "letter" => (
            "\\documentclass[uplatex,dvipdfmx,11pt,a4paper]{letter}\n",
            "\\laboheading",
            "\\labosubheading",
            "",
        ),
        file if file.ends_with(".tex") => {
            let path = match relative_file(project, file) {
                Ok(path) => project.join(path),
                Err(error) if Path::new(file).starts_with("templates") => {
                    crate::rules::resolve(project)?.template(project, file)?.ok_or(error)?
                }
                Err(error) => return Err(error),
            };
            let source = std::fs::read_to_string(&path)
                .with_context(|| format!("テンプレート {file} を読めない"))?;
            return fill_template(
                &source,
                &macros,
                &section_body(graph, structure, "\\section", "\\subsection"),
            )
            .with_context(|| format!("テンプレート {file}"));
        }
        _ => bail!("未知の書類テンプレート: {template}"),
    };
    let body = section_body(graph, structure, heading, subheading);
    let theorems = theorems(counter);
    let mut tex = format!(
        "{HEADER}{class}{MATH}{LINKS}\\usepackage{{pxjahyper}}\n{CLEVEREF}{macros}{theorems}{CREF_NAMES}"
    );
    if template == "letter" {
        tex.push_str(concat!(
            "\\newcommand{\\laboheading}[1]{\\par\\medskip\\noindent\\textbf{#1}\\par\\smallskip}\n",
            "\\newcommand{\\labosubheading}[1]{\\par\\smallskip\\noindent\\textbf{#1}\\par}\n"
        ));
        writeln!(
            tex,
            "\\signature{{{author}}}\n\\date{{{date}}}\n\\begin{{document}}\n             \\begin{{letter}}{{}}\n\\opening{{{title}}}\n{body}\\closing{{敬具}}\n\\end{{letter}}"
        ).unwrap();
    } else {
        writeln!(
            tex,
            "\\title{{{title}}}\n\\author{{{author}}}\n\\date{{{date}}}\n             \\begin{{document}}\n\\maketitle\n{body}"
        ).unwrap();
    }
    tex.push_str("\\end{document}\n");
    Ok(tex)
}

fn fill_template(source: &str, macros: &str, body: &str) -> Result<String> {
    let (head, tail) = source
        .split_once(BODY_MARKER)
        .context("%%LABO-BODY%% がない")?;
    ensure!(!tail.contains(BODY_MARKER), "%%LABO-BODY%% は 1 か所に置く");
    let head = if head.contains(PREAMBLE_MARKER) {
        head.replace(PREAMBLE_MARKER, macros)
    } else if macros.is_empty() {
        head.to_string()
    } else {
        let document = head
            .find("\\begin{document}")
            .context("\\begin{document} がない")?;
        format!("{}{macros}{}", &head[..document], &head[document..])
    };
    Ok(format!("{HEADER}{head}{body}{tail}"))
}

pub(crate) fn deck_tex(project: &Path, deck_rel: &str) -> Result<(&'static str, String)> {
    let deck = project.join(deck_rel);
    let cfg = DeckConfig::read_in_workspace(&deck, project)?;
    if cfg.meta.imported_latex {
        let (_, tex) = fuzukue_cards::latex_book::render(&deck, &cfg)?;
        return Ok((deck_target(&cfg.meta), tex));
    }
    let graph = Graph::open_in_project(&deck, &[], Some(project))?;
    let structure = read_structure(&deck, &cfg)?;
    let meta = &cfg.meta;
    let tex = match meta.kind.as_str() {
        "slides" => slides_tex(&deck, meta, &graph, structure)?,
        "document" => document_tex(project, meta, &graph, &structure)?,
        _ => paper_tex(meta, &graph, &structure),
    };
    Ok((deck_target(meta), tex))
}

pub(crate) fn deck_target(meta: &DeckMeta) -> &'static str {
    match meta.kind.as_str() {
        "slides" => "slides",
        "document" => "document",
        _ => "paper",
    }
}

pub(crate) fn uses_harness(cfg: &DeckConfig, graph: &Graph) -> bool {
    // Some folder-card decks also use harness.toml for their settings.
    cfg.harness
        && (cfg.html.html_out.is_some()
            || graph.cards.values().all(|card| card.external)
            || graph
                .cards
                .values()
                .any(|card| !card.external && !card.path.ends_with("card.md")))
}

pub(crate) fn read_structure(deck: &Path, cfg: &DeckConfig) -> Result<Value> {
    let path = deck.join(&cfg.structure);
    let source = std::fs::read_to_string(&path).with_context(|| {
        if cfg.structure == Path::new("structure.yaml") {
            "structure.yaml がない".to_owned()
        } else {
            format!("{} を読めない", path.display())
        }
    })?;
    serde_yaml::from_str(&source).with_context(|| format!("{} を解析できない", path.display()))
}

pub struct Built {
    pub ok: bool,
    pub pdf: String,
    pub log: String,
    pub log_path: Option<String>,
    pub tex_root: Option<String>,
    pub engine: Option<&'static str>,
    pub additional_pdfs: Vec<String>,
    pub pdf_audit: Vec<fuzukue_proto::PdfAudit>,
}

struct BuildOutputs {
    ok: bool,
    pdf: PathBuf,
    additional_pdfs: Vec<PathBuf>,
}

enum LatexMode<'a> {
    Generated(Option<Engine>),
    Harness(Engine),
    Imported(Engine),
    Source(Engine, &'a Path),
}

impl LatexMode<'_> {
    fn engine(&self) -> Engine {
        match *self {
            Self::Generated(engine) => engine.unwrap_or(Engine::UpLaTeX),
            Self::Harness(engine) | Self::Imported(engine) | Self::Source(engine, _) => engine,
        }
    }
}

const COMMON_ARGS: &[&str] = &["-synctex=1", "-interaction=nonstopmode", "-halt-on-error"];
const ALLOWED_EXTRA_ARGS: &[&str] = &[
    "-file-line-error",
    "-g",
    "-norc",
    "-quiet",
    "-recorder",
    "-silent",
    "-time",
    "-verbose",
    "-Werror",
];

fn validate_extra_args(extra: &[String]) -> Result<()> {
    for arg in extra {
        ensure!(
            ALLOWED_EXTRA_ARGS.contains(&arg.as_str()),
            "[build].latexmk_extra に対応していない引数: {arg:?}"
        );
    }
    Ok(())
}

fn latexmk_command(cwd: &Path, source: &Path, mode: &LatexMode<'_>, extra: &[String]) -> Command {
    let mut command = Command::new("latexmk");
    match *mode {
        LatexMode::Generated(None) => {
            command.args([
                "-pdfdvi",
                "-latex=uplatex -synctex=1 -interaction=nonstopmode -halt-on-error %O %S",
                "-e",
                "$dvipdf = q/dvipdfmx %O -o %D %S/",
            ]);
        }
        LatexMode::Generated(Some(engine)) | LatexMode::Harness(engine) => {
            command.args(engine.args()).args(COMMON_ARGS);
        }
        LatexMode::Imported(engine) => {
            command
                .args(Engine::COMMON_ARGS)
                .args(engine.args())
                .arg("-outdir=build");
        }
        LatexMode::Source(engine, out) => {
            let mut outdir = OsString::from("-outdir=");
            outdir.push(out);
            command
                .args(engine.args())
                .args(COMMON_ARGS)
                .arg("-file-line-error")
                .arg(outdir);
        }
    }
    if matches!(mode, LatexMode::Generated(_)) {
        command.arg("-file-line-error");
    }
    command.args(extra).arg(source).current_dir(cwd);
    command
}

fn source_command(root: &Path, out: &Path, engine: Engine, extra: &[String]) -> Result<Command> {
    let cwd = root.parent().context("TeX root のディレクトリがない")?;
    let name = root.file_name().context("TeX root のファイル名がない")?;
    // latexmk checks the source argument for TeX filename restrictions.
    let source = Path::new(".").join(name);
    Ok(latexmk_command(
        cwd,
        &source,
        &LatexMode::Source(engine, out),
        extra,
    ))
}

fn resolve_tex_root(deck: &Path, root: &Path) -> Result<PathBuf> {
    ensure!(
        root.is_relative() && !root.components().any(|c| c == Component::ParentDir),
        "[build].tex_root はデッキ内の相対パスで指定する: {}",
        root.display()
    );
    let full = deck
        .join(root)
        .canonicalize()
        .with_context(|| format!("[build].tex_root のファイルを開けない: {}", root.display()))?;
    ensure!(
        full.starts_with(deck),
        "[build].tex_root がデッキの外を指している: {}",
        root.display()
    );
    ensure!(
        full.is_file(),
        "[build].tex_root はファイルではない: {}",
        root.display()
    );
    // latexmk can substitute <path>.tex for an existing path with another suffix.
    ensure!(
        full.extension().is_some_and(|ext| ext == "tex"),
        "[build].tex_root は .tex ファイルで指定する: {}",
        root.display()
    );
    Ok(full)
}

#[derive(Default)]
struct BuildLog {
    tex_root: Option<String>,
    engine: Option<&'static str>,
    full: String,
    tail: String,
    diagnostics: Option<batch::Diagnostics>,
    recorded: Option<Vec<RecordedRun>>,
}

pub(crate) struct RecordedRun {
    pub(crate) cwd: PathBuf,
    pub(crate) log: PathBuf,
}

pub(crate) struct RecordedBuild {
    pub(crate) built: Built,
    pub(crate) runs: Vec<RecordedRun>,
    pub(crate) diagnostics: serde_json::Value,
}

impl BuildLog {
    fn line(&mut self, line: fmt::Arguments<'_>) {
        writeln!(self.full, "{line}").unwrap();
        writeln!(self.tail, "{line}").unwrap();
    }

    fn start(&mut self, root: &Path, engine: Engine) {
        self.tex_root
            .get_or_insert_with(|| root.to_string_lossy().into_owned());
        self.engine.get_or_insert(engine.name());
        self.line(format_args!(
            "tex_root: {root:?}\nengine: {}",
            engine.name()
        ));
    }

    fn record(&mut self, run: &std::process::Output) {
        let stdout = String::from_utf8_lossy(&run.stdout);
        let stderr = String::from_utf8_lossy(&run.stderr);
        let log = stdout.into_owned() + &stderr;
        writeln!(self.full, "{log}").unwrap();
        // Keep the TeX source line even when latexmk's summary pushes it out of the tail.
        let diagnostic = (!run.status.success()).then(|| {
            log.lines().find(|line| {
                line.split_once(".tex:").is_some_and(|(_, rest)| {
                    rest.split_once(':').is_some_and(|(number, _)| number.parse::<usize>().is_ok())
                })
            })
        }).flatten();
        // The two header lines count toward the 15-line log excerpt.
        let mut lines: Vec<_> = log.lines().rev().take(13).collect();
        if let Some(line) = diagnostic.filter(|line| !lines.contains(line)) {
            lines.pop();
            lines.push(line);
        }
        for line in lines.into_iter().rev() {
            writeln!(self.tail, "{line}").unwrap();
        }
    }

    async fn run(
        &mut self,
        command: &mut Command,
        log_path: &Path,
        cancel: &Cancellation,
    ) -> Result<std::process::Output> {
        if let Some(runs) = &mut self.recorded {
            let cwd = command.as_std().get_current_dir().context("TeX working directory")?;
            if !runs.iter().any(|run| run.log == log_path) {
                runs.push(RecordedRun { cwd: cwd.to_owned(), log: log_path.to_owned() });
                // Discard old intermediate files in the private build copy.
                command.arg("-gg");
            }
            command.arg("-recorder");
        } else if self.diagnostics.is_some() {
            // Start each target afresh, even when the snapshot includes an old build.
            command
                .arg("-g")
                .env("MISSFONT_LOG", "missfont.log")
                .env_remove("TEXMFOUTPUT");
            match tokio::fs::remove_file(log_path).await {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("{} を削除できない", log_path.display()))
                }
            }
        }
        let run = run_latex(command, cancel).await?;
        if self.diagnostics.is_none() || self.recorded.is_some() {
            self.record(&run);
        }
        Ok(run)
    }

    fn diagnose(&mut self, run: &std::process::Output, log_path: &Path) {
        if let Some(diagnostics) = &mut self.diagnostics {
            diagnostics.record(run, log_path);
        }
    }

    fn finish(
        mut self,
        deck: &Path,
        deck_rel: &Path,
        result: Result<BuildOutputs>,
    ) -> Result<Built> {
        let relative = |pdf: PathBuf| deck_rel.join(pdf).to_string_lossy().into_owned();
        let (ok, pdf, additional_pdfs) = match result {
            Ok(output) => (
                output.ok,
                relative(output.pdf),
                output.additional_pdfs.into_iter().map(relative).collect(),
            ),
            Err(error) if self.tex_root.is_none() || error.is::<Cancelled>() => return Err(error),
            Err(error) => {
                self.line(format_args!("{error:#}"));
                (false, String::new(), Vec::new())
            }
        };
        let log_path = if self.tex_root.is_some() {
            let path = Path::new("build/fuzukue-latexmk-output.txt");
            match self.save(&deck.join(path)) {
                Ok(()) => Some(deck_rel.join(path).to_string_lossy().into_owned()),
                Err(error) => {
                    self.line(format_args!("ビルドログを保存できない: {error:#}"));
                    None
                }
            }
        } else {
            None
        };
        Ok(Built {
            ok,
            pdf,
            log: self.tail,
            log_path,
            tex_root: self.tex_root,
            engine: self.engine,
            additional_pdfs,
            pdf_audit: Vec::new(),
        })
    }

    fn save(&self, path: &Path) -> Result<()> {
        let out = path.parent().context("ビルドログの出力先がない")?;
        ensure!(!out.is_symlink(), "build/ には実ディレクトリが必要");
        let mut file = tempfile::NamedTempFile::new_in(out)?;
        std::io::Write::write_all(&mut file, self.full.as_bytes())?;
        file.persist(path)?;
        Ok(())
    }
}

/// デッキ（プロジェクトからの相対パス）の PDF を作る。
#[cfg(test)]
pub async fn build(project: &Path, deck_rel: &str) -> Result<Built> {
    build_cancellable(project, deck_rel, &Cancellation::default()).await
}

pub(crate) async fn build_cancellable(
    project: &Path,
    deck_rel: &str,
    cancel: &Cancellation,
) -> Result<Built> {
    cancel.check()?;
    let deck = fuzukue_cards::workspace_paths::resolve(project, project, Path::new(deck_rel))?;
    let mut log = BuildLog::default();
    let result = build_deck(project, deck_rel, &deck, cancel, &mut log).await;
    log.finish(&deck, Path::new(deck_rel), result)
}

/// Use the ordinary renderer in a private copy, retaining recorder and diagnostic artifacts.
pub(crate) async fn build_recorded(project: &Path, deck_rel: &str, cancel: &Cancellation) -> Result<RecordedBuild> {
    let deck = fuzukue_cards::workspace_paths::resolve(project, project, Path::new(deck_rel))?;
    let mut log = BuildLog {
        diagnostics: Some(batch::Diagnostics::default()),
        recorded: Some(Vec::new()),
        ..BuildLog::default()
    };
    let result = build_deck(project, deck_rel, &deck, cancel, &mut log).await;
    let diagnostics = serde_json::to_value(log.diagnostics.as_ref())?;
    let runs = log.recorded.take().unwrap_or_default();
    let built = log.finish(&deck, Path::new(deck_rel), result)?;
    Ok(RecordedBuild { built, runs, diagnostics })
}

async fn build_deck(
    project: &Path,
    deck_rel: &str,
    deck: &Path,
    cancel: &Cancellation,
    log: &mut BuildLog,
) -> Result<BuildOutputs> {
    let cfg = DeckConfig::read_in_workspace(deck, project)?;
    let settings = cfg.meta.build.as_ref();
    log.engine = settings.and_then(|build| build.engine).map(Engine::name);
    let extra = settings
        .map(|build| build.latexmk_extra.as_slice())
        .unwrap_or_default();
    validate_extra_args(extra)?;
    if let Some(root) = settings.and_then(|build| build.tex_root.as_deref()) {
        let engine = settings
            .and_then(|build| build.engine)
            .unwrap_or(Engine::UpLaTeX);
        log.engine = Some(engine.name());
        let deck = deck.canonicalize()?;
        let source = resolve_tex_root(&deck, root)?;
        let root = source.strip_prefix(&deck)?;
        log.start(&Path::new(deck_rel).join(root), engine);
        let out = deck.join("build");
        ensure!(!out.is_symlink(), "build/ には実ディレクトリが必要");
        tokio::fs::create_dir_all(&out).await?;
        let mut command = source_command(&source, &out, engine, extra)?;
        let name = source.file_name().context("TeX root のファイル名がない")?;
        let log_path = out.join(name).with_extension("log");
        let run = log.run(&mut command, &log_path, cancel).await?;
        log.diagnose(&run, &log_path);
        return Ok(BuildOutputs {
            ok: run.status.success(),
            pdf: Path::new("build").join(name).with_extension("pdf"),
            additional_pdfs: Vec::new(),
        });
    }
    if cfg.meta.imported_latex {
        let engine = fuzukue_cards::latex_book::prepare_build(deck, &cfg)?;
        let source = Path::new("build/paper.tex");
        let mut command = latexmk_command(deck, source, &LatexMode::Imported(engine), extra);
        log.start(&Path::new(deck_rel).join(source), engine);
        let log_path = deck.join(source).with_extension("log");
        let run = log.run(&mut command, &log_path, cancel).await?;
        log.diagnose(&run, &log_path);
        return Ok(BuildOutputs {
            ok: run.status.success(),
            pdf: source.with_extension("pdf"),
            additional_pdfs: Vec::new(),
        });
    }
    if cfg.harness {
        let graph = Graph::load_deck(deck, &cfg, project)?;
        if uses_harness(&cfg, &graph) {
            return harness::build(project, deck, deck_rel, &cfg, &graph, cancel, log).await;
        }
    }
    let mode = LatexMode::Generated(settings.and_then(|build| build.engine));
    log.engine = Some(mode.engine().name());
    let (name, tex) = deck_tex(project, deck_rel)?;
    let out = deck.join("build");
    ensure!(!out.is_symlink(), "build/ には実ディレクトリが必要");
    let filename = format!("{name}.tex");
    let root = Path::new("build").join(&filename);
    log.start(&Path::new(deck_rel).join(&root), mode.engine());
    tokio::fs::create_dir_all(&out).await?;
    tokio::fs::write(out.join(&filename), tex).await?;
    let mut command = latexmk_command(&out, Path::new(&filename), &mode, extra);
    let log_path = out.join(&filename).with_extension("log");
    let previous = tempfile::tempdir_in(&out)?;
    let artifacts = [format!("{name}.pdf"), format!("{name}.synctex.gz")];
    for artifact in &artifacts {
        match tokio::fs::symlink_metadata(out.join(artifact)).await {
            Ok(info) => {
                ensure!(info.is_file(), "build artifact is not a regular file: {artifact}");
                let backup = previous.path().join(format!("{artifact}.previous"));
                tokio::fs::copy(out.join(artifact), &backup).await?;
                std::fs::File::open(backup)?.set_modified(info.modified()?)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    let run = log.run(&mut command, &log_path, cancel).await;
    if !run.as_ref().is_ok_and(|run| run.status.success()) {
        for artifact in &artifacts {
            let path = out.join(artifact);
            let backup = previous.path().join(format!("{artifact}.previous"));
            if backup.is_file() {
                tokio::fs::rename(backup, path).await?;
            } else if let Err(error) = tokio::fs::remove_file(path).await {
                if error.kind() != std::io::ErrorKind::NotFound {
                    return Err(error.into());
                }
            }
        }
    }
    let run = run?;
    log.diagnose(&run, &log_path);
    Ok(BuildOutputs {
        ok: run.status.success(),
        pdf: root.with_extension("pdf"),
        additional_pdfs: Vec::new(),
    })
}

// latexmk also starts TeX and PDF converters. Stop the whole process group
// and wait for it to release its output before allowing the next build.
pub(crate) async fn run_latex(
    command: &mut Command,
    cancel: &Cancellation,
) -> Result<std::process::Output> {
    use std::process::Stdio;

    cancel.check()?;
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let (child, job) = crate::job_process::Job::spawn(command, "latex", None)
        .context("latexmk を起動できない")?;
    let output = child.wait_with_output();
    tokio::pin!(output);
    tokio::select! {
        biased;
        _ = cancel.cancelled() => {
            job.stop().context("latexmk を停止できない")?;
            output.await.context("latexmk の終了を待てない")?;
            Err(crate::cancellation::Cancelled.into())
        }
        result = &mut output => result.context("latexmk の結果を読めない"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestDir;

    fn assert_command(command: &Command, cwd: &Path, args: &[&str]) {
        let command = command.as_std();
        assert_eq!(command.get_program(), "latexmk");
        assert_eq!(command.get_current_dir(), Some(cwd));
        assert_eq!(command.get_args().collect::<Vec<_>>(), args);
    }

    #[test]
    fn unchanged_default_latexmk_commands() -> Result<()> {
        let dir = TestDir::new();
        dir.write("deck.toml", "kind = 'paper'\n");
        let cfg = DeckConfig::read(dir.path())?;
        let build = cfg.meta.build.unwrap_or_default();
        assert!(build.tex_root.is_none());
        let cwd = Path::new("/deck/build");
        let source = Path::new("paper.tex");
        assert_command(
            &latexmk_command(
                cwd,
                source,
                &LatexMode::Generated(build.engine),
                &build.latexmk_extra,
            ),
            cwd,
            &[
                "-pdfdvi",
                "-latex=uplatex -synctex=1 -interaction=nonstopmode -halt-on-error %O %S",
                "-e",
                "$dvipdf = q/dvipdfmx %O -o %D %S/",
                "-file-line-error",
                "paper.tex",
            ],
        );
        assert_command(
            &latexmk_command(cwd, Path::new("cards.tex"), &LatexMode::Harness(Engine::UpLaTeX), &[]),
            cwd,
            &[
                "-pdfdvi",
                "-e",
                "$latex = 'uplatex %O %S'; $bibtex = 'upbibtex %O %B'; $dvipdf = 'dvipdfmx %O -o %D %S';",
                "-synctex=1",
                "-interaction=nonstopmode",
                "-halt-on-error",
                "cards.tex",
            ],
        );
        let cwd = Path::new("/deck");
        assert_command(
            &latexmk_command(
                cwd,
                Path::new("build/paper.tex"),
                &LatexMode::Imported(Engine::PdfLaTeX),
                &[],
            ),
            cwd,
            &[
                "-norc",
                "-no-shell-escape",
                "-interaction=nonstopmode",
                "-halt-on-error",
                "-synctex=1",
                "-pdf",
                "-outdir=build",
                "build/paper.tex",
            ],
        );
        Ok(())
    }

    #[test]
    fn configured_engines_and_root_select_latexmk_arguments() -> Result<()> {
        let dir = TestDir::new();
        dir.write("manuscript/draft.tex", "");
        let out = dir.path().join("build");
        let out_argument = format!("-outdir={}", out.display());
        for (name, flags) in [
            (
                "uplatex-dvipdfmx",
                &[
                    "-pdfdvi",
                    "-e",
                    "$latex = 'uplatex %O %S'; $bibtex = 'upbibtex %O %B'; $dvipdf = 'dvipdfmx %O -o %D %S';",
                ][..],
            ),
            ("lualatex", &["-lualatex"]),
            ("pdflatex", &["-pdf"]),
            ("xelatex", &["-xelatex"]),
        ] {
            dir.write("deck.toml", &format!(
                "[build]\ntex_root = 'manuscript/draft.tex'\nengine = '{name}'\nlatexmk_extra = ['-g', '-time']\n"
            ));
            let settings = DeckConfig::read(dir.path())?.meta.build.unwrap();
            let root = resolve_tex_root(dir.path(), settings.tex_root.as_deref().unwrap())?;
            assert_eq!(root, dir.path().join("manuscript/draft.tex"));
            let engine = settings.engine.unwrap();
            assert_eq!(engine.name(), name);
            let command = source_command(&root, &out, engine, &settings.latexmk_extra)?;
            let mut expected = flags.to_vec();
            expected.extend([
                "-synctex=1",
                "-interaction=nonstopmode",
                "-halt-on-error",
                "-file-line-error",
                &out_argument,
                "-g",
                "-time",
                "./draft.tex",
            ]);
            assert_command(&command, &dir.path().join("manuscript"), &expected);
        }
        for invalid in ["platex", "unknown"] {
            dir.write("deck.toml", &format!("[build]\nengine = '{invalid}'\n"));
            assert!(DeckConfig::read(dir.path()).is_err());
        }
        Ok(())
    }

    #[test]
    fn rejects_invalid_tex_roots_and_extra_flags() -> Result<()> {
        let dir = TestDir::new();
        dir.write("project/deck/inside.tex", "");
        dir.write("project/deck/not-tex.txt", "");
        dir.write("project/other.tex", "");
        let deck = dir.path().join("project/deck");
        std::os::unix::fs::symlink("../other.tex", deck.join("escaped.tex"))?;
        std::os::unix::fs::symlink("..", deck.join("escaped-dir"))?;
        let absolute = deck.join("inside.tex");
        for (path, message) in [
            (Path::new("missing.tex"), "ファイルを開けない"),
            (Path::new("../other.tex"), "相対パス"),
            (Path::new("../deck/inside.tex"), "相対パス"),
            (&absolute, "相対パス"),
            (Path::new("escaped.tex"), "デッキの外"),
            (Path::new("escaped-dir/other.tex"), "デッキの外"),
            (Path::new("."), "ファイルではない"),
            (Path::new("not-tex.txt"), ".tex ファイル"),
        ] {
            let err = resolve_tex_root(&deck, path).unwrap_err();
            assert!(err.to_string().contains(message), "{err:#}");
        }
        validate_extra_args(
            &ALLOWED_EXTRA_ARGS
                .iter()
                .map(|arg| (*arg).to_owned())
                .collect::<Vec<_>>(),
        )?;
        for invalid in [
            "-g;true",
            "$(true)",
            "-g\n-time",
            "-e",
            "-r",
            "-shell-escape",
            "-outdir=elsewhere",
            "other.tex",
        ] {
            let err = validate_extra_args(&[invalid.into()]).unwrap_err();
            assert!(err.to_string().contains("latexmk_extra"), "{err:#}");
        }
        Ok(())
    }

    #[tokio::test]
    async fn build_paper_fixture_reports_artifacts_failure_and_rollback() -> Result<()> {
        use std::sync::Arc;

        use axum::{
            body::to_bytes,
            extract::{Query, State},
            Json,
        };
        use serde_json::{json, Value};

        if !crate::test_support::tex::ready("uplatex", &["latexmk", "dvipdfmx"])? {
            return Ok(());
        }

        let dir = TestDir::new();
        let config = "title = 'PDF fixture'\nkind = 'paper'\n";
        dir.write("deck/deck.toml", config);
        dir.write(
            "deck/structure.yaml",
            "sections: [{title: 組版, items: []}]\n",
        );
        let app = Arc::new(crate::Workspace::new(
            dir.path().to_owned(),
            String::new(),
            dir.path().join(".fuzukue"),
            Arc::new(crate::hub::Hub::new()),
        ));
        let build_paper = async || -> Result<Value> {
            let response = crate::mcp::handler(
                State(app.clone()),
                Query(crate::mcp::McpQuery {
                    session: "test".into(),
                    backend: None,
                    workspace: None,
                    job: None,
                }),
                axum::http::HeaderMap::new(),
                Json(json!({
                    "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                    "params": { "name": "build_paper", "arguments": { "deck": "deck" } },
                })),
            )
            .await;
            let body: Value =
                serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await?)?;
            Ok(body["result"].clone())
        };
        let result = build_paper().await?;
        let built = &result["structuredContent"];
        assert_eq!(result["isError"], false, "{result}");
        assert_eq!(built["tex_root"], "deck/build/paper.tex");
        assert_eq!(built["engine"], "uplatex-dvipdfmx");
        assert_eq!(built["pdf"], "deck/build/paper.pdf");
        assert!(dir.path().join("deck/build/paper.pdf").metadata()?.len() > 0);
        assert!(
            dir.path()
                .join("deck/build/paper.synctex.gz")
                .metadata()?
                .len()
                > 0
        );

        dir.write("deck/deck.toml", &format!(
            "{config}[build]\ntex_root = 'manuscript/draft.tex'\nengine = 'uplatex-dvipdfmx'\nlatexmk_extra = ['-time']\n"
        ));
        let root = "deck/manuscript/draft.tex";
        dir.write(
            root,
            r"\documentclass[uplatex]{jsarticle}
\begin{document}
\input{body}
\end{document}
",
        );
        dir.write("deck/manuscript/body.tex", "原稿のテスト\n");
        dir.write(
            "deck/manuscript/.latexmkrc",
            "$pdf_mode = 1;\nprint \"fixture rc loaded\\n\";\n",
        );
        let result = build_paper().await?;
        let built = &result["structuredContent"];
        assert_eq!(result["isError"], false, "{result}");
        assert_eq!(built["tex_root"], root);
        assert_eq!(built["pdf"], "deck/build/draft.pdf");
        assert!(dir.path().join("deck/build/draft.pdf").metadata()?.len() > 0);
        assert!(
            dir.path()
                .join("deck/build/draft.synctex.gz")
                .metadata()?
                .len()
                > 0
        );

        dir.write("deck/manuscript/body.tex", "\\undefinedFixtureCommand\n");
        let failure = build_paper().await?;
        let failed = &failure["structuredContent"];
        assert_eq!(failure["isError"], true, "{failure}");
        assert_eq!(failed["ok"], false);
        assert_eq!(failed["tex_root"], root);
        assert_eq!(failed["engine"], "uplatex-dvipdfmx");
        let header = format!("tex_root: {root:?}\nengine: uplatex-dvipdfmx\n");
        let log_path = failed["log_path"].as_str().context("missing log_path")?;
        let full_log = std::fs::read_to_string(dir.path().join(log_path))?;
        assert!(full_log.starts_with(&header), "{full_log}");
        assert!(full_log.contains("fixture rc loaded"), "{full_log}");
        assert!(
            full_log.contains("Undefined control sequence"),
            "{full_log}"
        );
        assert!(failed["log"].as_str().unwrap().starts_with(&header));

        std::fs::remove_file(dir.path().join(root))?;
        let missing = build_paper().await?;
        assert_eq!(missing["isError"], true, "{missing}");
        assert!(missing["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("[build].tex_root"));

        dir.write("deck/deck.toml", config);
        let rollback = build_paper().await?;
        assert_eq!(rollback["isError"], false, "{rollback}");
        assert_eq!(
            rollback["structuredContent"]["tex_root"],
            "deck/build/paper.tex"
        );
        assert_eq!(rollback["structuredContent"]["engine"], "uplatex-dvipdfmx");
        Ok(())
    }
}
