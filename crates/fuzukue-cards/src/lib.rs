//! カードハーネスの核。
//!
//! カード = YAML frontmatter つきの Markdown ファイル。`id` を持ち、
//! frontmatter の `links` / `requires` と本文中の `[[id]]` で他のカードを、
//! `files` と本文中の `[[file:path]]` でリポジトリ内のファイルを指す。
//! ここではカードを読み込み、グラフ（前向き・後ろ向き）とファイル→カードの
//! 逆引きを作り、整合性を調べ、作業の入口カードから読むべきものを選ぶ。
//!
//! 既存の `~/research` の論文カード（`requires: [id, ...]`）もそのまま読める。

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use anyhow::{Context, Result};
use regex::Regex;
use serde::{Deserialize, Serialize};

mod markdown;
mod translation;
pub mod latex_book;
pub mod course;
pub mod reading_sync;
pub use markdown::section_range;
mod style_lint;
mod term_lint;
pub mod step_ledger;
pub mod workspace_paths;

pub use style_lint::StyleConfig;
pub use term_lint::prh;
pub use markdown::{merge_card_edit, CardPart};
pub use translation::{source_fingerprint, with_source_fingerprint};

/// 依存として辿る関係。これ以外（`see` など）は参照だけで、文脈には自動で入れない。
pub const FOLLOW_RELS: &[&str] = &["requires", "refines", "implements"];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Link {
    pub to: String,
    #[serde(default = "default_rel")]
    pub rel: String,
}

fn default_rel() -> String {
    "see".into()
}

#[derive(Debug, Deserialize)]
struct Front {
    id: String,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    status: Option<String>,
    label: Option<String>,
    summary: Option<serde_yaml::Value>,
    role: Option<String>,
    #[serde(default)]
    explains: Option<Vec<String>>,
    #[serde(default)]
    links: Vec<Link>,
    // Step cards distinguish an explicitly empty list from an omitted list.
    #[serde(default, deserialize_with = "declared_requires")]
    requires: Option<Vec<String>>,
    #[serde(default)]
    files: Vec<String>,
    #[serde(default)]
    issues: Vec<String>,
    /// 出典。文字列（`books/x.pdf#123 Theorem 2.4` など）か、{pdf, page, item} の表。
    #[serde(default)]
    source: Option<serde_yaml::Value>,
    #[serde(default)]
    additional_sources: Vec<String>,
    #[serde(default)]
    proof_sources: Vec<String>,
    #[serde(default)]
    source_sha256: BTreeMap<String, String>,
    source_id: Option<String>,
    source_fingerprint: Option<String>,
    from: Option<String>,
    from_thread: Option<String>,
    /// このカードを形式化した Lean の宣言（`lean/` のパッケージ。`LabSample.hamming_le_length` など）
    #[serde(default)]
    lean: Vec<String>,
    #[serde(default)]
    defines: Option<Vec<String>>,
    #[serde(default)]
    lint_ignore: Vec<String>,
}

fn declared_requires<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<Vec<String>>, D::Error> {
    Vec::deserialize(deserializer).map(Some)
}

/// 出典を 1 行の文字列にそろえる。表なら `pdf#page item` に。
fn source_string(v: &serde_yaml::Value) -> Option<String> {
    match v {
        serde_yaml::Value::String(s) => Some(s.clone()),
        serde_yaml::Value::Mapping(_) => {
            let pdf = v["pdf"].as_str()?;
            let mut s = pdf.to_string();
            if let Some(p) = v["page"].as_u64() {
                s.push_str(&format!("#{p}"));
            }
            if let Some(item) = v["item"].as_str() {
                s.push(' ');
                s.push_str(item);
            }
            Some(s)
        }
        _ => None,
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Card {
    pub id: String,
    /// 別のデッキから `ns:id` として取り込んだカード。検査の対象外。
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub external: bool,
    pub kind: String,
    pub title: String,
    pub status: String,
    #[serde(skip)]
    pub summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    pub step: Option<step_ledger::StepMetadata>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub explains: Vec<String>,
    pub links: Vec<Link>,
    pub files: Vec<String>,
    pub issues: Vec<String>,
    pub source: Option<String>,
    #[serde(skip)]
    additional_sources: Vec<String>,
    #[serde(skip)]
    proof_sources: Vec<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub source_sha256: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_fingerprint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from_thread: Option<String>,
    pub lean: Vec<String>,
    /// 明示した定義語、または定義カードの主張から推定した語。
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub defines: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub lint_ignore: Vec<String>,
    /// フォルダのカードなら、本文として読んだ TeX のファイル（リポジトリ根からの相対）
    pub tex: Vec<PathBuf>,
    /// リポジトリ根からの相対パス。
    pub path: PathBuf,
    pub body: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub statement: Option<CardPart>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proof: Option<CardPart>,
    #[serde(skip)]
    pub prose: Option<CardPart>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Error,
    Warning,
}

#[derive(Debug, Clone, Serialize)]
pub struct Diagnostic {
    pub severity: Severity,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<&'static str>,
    pub path: PathBuf,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub textlint_rule: Option<String>,
    /// One-based position in the assembled card body.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub column: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fix_hint: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub style_note: Option<fuzukue_proto::CardStyleNote>,
}

impl Diagnostic {
    fn new(severity: Severity, code: Option<&'static str>, path: PathBuf, message: String) -> Self {
        Self { severity, code, path, message, textlint_rule: None, line: None, column: None, fix_hint: None, style_note: None }
    }

    pub fn display_message(&self) -> String {
        if let Some(code) = self.code.filter(|code| code.starts_with("style/")) {
            fuzukue_i18n::Locale::JAPANESE.style_diagnostic(code, self.textlint_rule.as_deref(), &self.message)
        } else if let Some(rule) = &self.textlint_rule {
            format!("(textlint: {rule}) {}", self.message)
        } else {
            self.message.clone()
        }
    }
}

#[derive(Debug)]
pub struct DeckConfig {
    pub dirs: Vec<PathBuf>,
    /// `harness.toml` が設定ファイルとして選ばれたデッキ。
    pub harness: bool,
    pub structure: PathBuf,
    pub macros_file: Option<PathBuf>,
    /// 名前空間 → 外部デッキの根（このデッキの根からの相対）
    external: BTreeMap<String, PathBuf>,
    external_harnesses: BTreeMap<String, PathBuf>,
    path_base: Option<PathBase>,
    pub html: HarnessHtml,
    /// `deck.toml` の中身（研究・原稿・本の区別や題など）。古い設定ファイルなら既定値。
    pub meta: DeckMeta,
    step_ledger: step_ledger::Config,
    pub lint: LintConfig,
    pub style: StyleConfig,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum PathBase {
    Workspace,
}

#[derive(Debug, Default, Deserialize)]
pub struct HarnessHtml {
    pub html_out: Option<PathBuf>,
    pub mathjax: Option<String>,
    #[serde(default)]
    pub html_links: Vec<HtmlLink>,
    pub html_index: Option<HtmlIndex>,
    pub reader_font_px: Option<u16>,
    pub reader_width_em: Option<u16>,
}

#[derive(Debug, Deserialize)]
pub struct HtmlLink {
    pub href: String,
    pub text: String,
}

#[derive(Debug, Deserialize)]
pub struct HtmlIndex {
    pub title: Option<String>,
    #[serde(default)]
    pub links: Vec<HtmlLink>,
}

#[derive(Debug, Default, Deserialize)]
pub struct LintConfig {
    #[serde(default)]
    pub ignore_terms: Vec<String>,
    pub glossary: Option<PathBuf>,
}

#[derive(Default, Deserialize)]
struct LintLayer {
    ignore_terms: Option<Vec<String>>,
    glossary: Option<PathBuf>,
}

impl LintLayer {
    fn apply(self, lint: &mut LintConfig, root: &Path) {
        if let Some(glossary) = self.glossary {
            lint.glossary = Some(root.join(glossary));
        }
        if let Some(terms) = self.ignore_terms {
            lint.ignore_terms = terms;
        }
    }
}

/// List a rule layer's direct files in filename order, rejecting symbolic links.
pub fn rule_files(root: &Path, dir: &str, extension: &str) -> Result<Vec<PathBuf>> {
    anyhow::ensure!(Path::new(dir).file_name() == Some(dir.as_ref()), "invalid rule directory name");
    let dir = root.join(dir);
    let metadata = match std::fs::symlink_metadata(&dir) {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("{} を読めない", dir.display())),
    };
    anyhow::ensure!(metadata.is_dir(), "{} は規則のディレクトリではない", dir.display());
    let mut files = Vec::new();
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == extension) {
            anyhow::ensure!(entry.file_type()?.is_file(), "{} は規則のファイルではない", path.display());
            files.push(path);
        }
    }
    files.sort();
    Ok(files)
}

fn apply_rule_layers(roots: &[PathBuf], lint: &mut LintConfig, style: &mut StyleConfig) -> Result<()> {
    #[derive(Deserialize)]
    struct File {
        lint: Option<LintLayer>,
        #[serde(flatten)]
        settings: LintLayer,
    }
    for root in roots {
        for path in rule_files(root, "style", "toml")? {
            style.overlay_pack(&std::fs::read_to_string(&path)?)
                .with_context(|| format!("{} を読めない", path.display()))?;
        }
        for path in rule_files(root, "lint", "toml")? {
            let file: File = toml::from_str(&std::fs::read_to_string(&path)?)
                .with_context(|| format!("{} を読めない", path.display()))?;
            file.lint.unwrap_or(file.settings).apply(lint, root);
        }
    }
    Ok(())
}

/// デッキの性質（`deck.toml`）。
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct DeckMeta {
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub imported_latex: bool,
    /// Main file of the original manuscript, relative to the deck directory.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub manuscript_main: Option<PathBuf>,
    /// "research" / "paper" / "book" / "notes" / "slides" / "document"。空なら区別なし。
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preamble: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ref_command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bibliography: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub build: Option<HarnessBuild>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub appendix_title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub date: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub template: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slides: Option<SlideOptions>,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub status: String,
    /// 次にやること
    #[serde(default)]
    pub next: String,
    /// 研究のデッキが扱う原稿・本のデッキ（このデッキの根からの相対パス）
    #[serde(default)]
    pub papers: Vec<PathBuf>,
    /// できあがる PDF など（このデッキの根からの相対パス）
    #[serde(default)]
    pub outputs: Vec<PathBuf>,
    /// 数式の、引数なしのマクロ
    #[serde(default)]
    #[serde(skip)]
    pub macros: BTreeMap<String, String>,
    /// 引数つきのマクロ（`Gm = ["\\Gamma_{#1}", 1]` の形）。名前 → (定義, 引数の数)
    #[serde(skip)]
    pub macros_with_args: BTreeMap<String, (String, u8)>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct HarnessBuild {
    pub auto: Option<bool>,
    #[serde(default)]
    pub tex: Vec<TexTarget>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tex_root: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub engine: Option<latex_book::Engine>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub latexmk_extra: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum TexTarget {
    File(String),
    Part { out: String, sections: Option<usize> },
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct SlideOptions {
    pub theme: Option<String>,
    #[serde(alias = "aspectratio")]
    pub aspect_ratio: Option<AspectRatio>,
    pub title: Option<String>,
    pub author: Option<String>,
    pub date: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum AspectRatio {
    Code(u16),
    Text(String),
}

impl DeckConfig {
    /// 設定は `deck.toml`（新しい形）、`cards.toml`、`harness.toml`（~/research の論文ハーネス）の順に探す。
    pub fn read(root: &Path) -> Result<Self> {
        Self::read_with_rules(root, &[])
    }

    fn read_with_rules(root: &Path, rules: &[PathBuf]) -> Result<Self> {
        Self::read_config(root, rules, None)
    }

    /// Read the selected manifest with the workspace's path boundary.
    pub fn read_in_workspace(root: &Path, workspace: &Path) -> Result<Self> {
        Self::read_config(root, &[], Some(workspace))
    }

    fn read_config(root: &Path, rules: &[PathBuf], workspace: Option<&Path>) -> Result<Self> {
        Self::parse_config(root, rules, |name| {
            let path = match workspace {
                Some(workspace) => workspace_paths::resolve(workspace, root, Path::new(name))?,
                None => root.join(name),
            };
            if path.exists() {
                Ok(Some(std::fs::read_to_string(&path)
                    .with_context(|| format!("{} を読めない", path.display()))?))
            } else {
                Ok(None)
            }
        })
    }

    /// Read a deck configuration through a caller-supplied reader. The reader
    /// receives manifest names in precedence order and returns `None` for a missing file.
    pub fn read_with(root: &Path, read: impl FnMut(&str) -> Result<Option<String>>) -> Result<Self> {
        Self::parse_config(root, &[], read)
    }

    fn parse_config(
        root: &Path,
        rules: &[PathBuf],
        mut read: impl FnMut(&str) -> Result<Option<String>>,
    ) -> Result<Self> {
        #[derive(Deserialize, Default)]
        struct Raw {
            #[serde(default)]
            dirs: Vec<PathBuf>,
            cards_dir: Option<PathBuf>,
            structure: Option<PathBuf>,
            #[serde(default)]
            external: BTreeMap<String, PathBuf>,
            #[serde(default)]
            external_harnesses: BTreeMap<String, PathBuf>,
            path_base: Option<PathBase>,
            #[serde(flatten)]
            html: HarnessHtml,
            #[serde(default)]
            mathjax_macros: BTreeMap<String, toml::Value>,
            /// deck.toml では表（名前 → 定義）。harness.toml では `macros = "macros.tex"`（ファイルの場所）なので、表のときだけ読む。
            #[serde(default)]
            macros: Option<toml::Value>,
            #[serde(default)]
            lint: LintLayer,
            #[serde(default)]
            style: StyleConfig,
            #[serde(default)]
            step_ledger: step_ledger::Config,
            #[serde(flatten)]
            meta: DeckMeta,
        }
        let mut raw = Raw::default();
        let mut harness = false;
        for name in ["deck.toml", "cards.toml", "harness.toml"] {
            if let Some(source) = read(name)? {
                raw = toml::from_str(&source)
                    .with_context(|| format!("{} を読めない", root.join(name).display()))?;
                harness = name == "harness.toml";
                break;
            }
        }
        let dirs = if raw.dirs.is_empty() {
            vec![raw.cards_dir.unwrap_or_else(|| "cards".into())]
        } else {
            raw.dirs
        };
        let mut meta = raw.meta;
        meta.bibliography = meta.bibliography.filter(|path| !path.as_os_str().is_empty());
        let macros_file = raw.macros.as_ref().and_then(toml::Value::as_str).map(PathBuf::from);
        // マクロは文字列（引数なし）か [定義, 引数の数] の配列。どちらの表にも書ける。
        let table: BTreeMap<String, toml::Value> = match raw.macros {
            Some(toml::Value::Table(t)) => t.into_iter().collect(),
            _ => BTreeMap::new(),
        };
        for (name, v) in table.into_iter().chain(raw.mathjax_macros) {
            match &v {
                toml::Value::String(body) => {
                    meta.macros.insert(name, body.clone());
                }
                toml::Value::Array(a) => match (a.first().and_then(|b| b.as_str()), a.get(1).and_then(|n| n.as_integer())) {
                    (Some(body), Some(0) | None) => {
                        meta.macros.insert(name, body.to_string());
                    }
                    (Some(body), Some(n)) if (1..=9).contains(&n) => {
                        meta.macros_with_args.insert(name, (body.to_string(), n as u8));
                    }
                    _ => {}
                },
                _ => {}
            }
        }
        let mut lint = LintConfig::default();
        let mut style = StyleConfig::default();
        apply_rule_layers(rules, &mut lint, &mut style)?;
        raw.lint.apply(&mut lint, root);
        style.overlay(raw.style);
        Ok(Self {
            dirs,
            harness,
            structure: raw.structure.unwrap_or_else(|| "structure.yaml".into()),
            macros_file,
            external: raw.external,
            external_harnesses: raw.external_harnesses,
            path_base: raw.path_base,
            html: raw.html,
            meta,
            step_ledger: raw.step_ledger,
            lint,
            style,
        })
    }

    fn harness_path_base<'a>(&self, workspace: &'a Path, deck: &'a Path) -> &'a Path {
        match self.path_base {
            Some(PathBase::Workspace) => workspace,
            None => deck,
        }
    }

    /// Resolve the viewer directory using the selected manifest's path base.
    pub fn html_dir(&self, workspace: &Path, deck: &Path) -> Result<PathBuf> {
        let path = self.html.html_out.as_deref()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or(Path::new("html"));
        workspace_paths::resolve(workspace, self.harness_path_base(workspace, deck), path)
            .with_context(|| format!("html_out = {}", path.display()))
    }

    /// Resolve each external namespace against its configured base.
    pub fn external_roots<'a>(
        &'a self,
        workspace: &'a Path,
        deck: &'a Path,
    ) -> impl Iterator<Item = (&'a str, Result<PathBuf>)> + 'a {
        let harness_base = self.harness_path_base(workspace, deck);
        self.external.iter()
            .filter(|(ns, _)| !self.external_harnesses.contains_key(*ns))
            .map(move |(ns, path)| {
                // Ordinary decks may link to a separate repository through [external].
                let root = if self.harness {
                    workspace_paths::resolve(workspace, deck, path)
                } else {
                    Ok(deck.join(path))
                };
                (ns.as_str(), root.with_context(|| format!("external deck `{ns}`: {}", path.display())))
            })
            .chain(self.external_harnesses.iter().map(move |(ns, path)| {
                let root = workspace_paths::resolve(workspace, harness_base, path)
                    .with_context(|| format!("external deck `{ns}`: {}", path.display()));
                (ns.as_str(), root)
            }))
    }
}

/// `root` の下で `deck.toml` を持つディレクトリを探す（.git や target などは見ない）。
pub fn find_decks(root: &Path) -> Vec<PathBuf> {
    const SKIP: &[&str] = &[".git", "target", ".lake", "node_modules", ".temp"];
    let mut out: Vec<PathBuf> = walkdir::WalkDir::new(root)
        .into_iter()
        .filter_entry(|e| {
            !fuzukue_proto::is_thread_save_artifact(e.path())
                && !e.path().strip_prefix(root).is_ok_and(fuzukue_proto::is_inbox_upload_path)
                && !e.file_name().to_str().is_some_and(|s| SKIP.contains(&s)
                    || s.starts_with(".fuzukue-research-") || s.starts_with(".fuzukue-book-import-"))
        })
        .filter_map(Result::ok)
        // deck.toml のほか、writing-harness の論文（harness.toml）と古い cards.toml も
        // デッキとして数える。DeckConfig::read と同じ並び。
        .filter(|e| {
            e.file_type().is_file()
                && matches!(e.file_name().to_str(), Some("deck.toml" | "harness.toml" | "cards.toml"))
        })
        .filter_map(|e| e.path().parent().map(Path::to_path_buf))
        .collect();
    out.sort();
    out.dedup();
    out
}

#[derive(Debug, Default)]
pub struct Graph {
    pub root: PathBuf,
    pub cards: BTreeMap<String, Card>,
    /// to → [(from, rel)]
    pub backlinks: HashMap<String, Vec<(String, String)>>,
    /// リポジトリ相対のファイルパス → それを指すカード
    pub file_refs: BTreeMap<String, BTreeSet<String>>,
    pub diagnostics: Vec<Diagnostic>,
    /// カード ID → (研究デッキの絶対パス, 元カード ID)
    pub research_origins: BTreeMap<String, (PathBuf, String)>,
}

// [[id]] と [[id|表示名]]。[[file:...]] は除く。
static INLINE_CARD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[\[([A-Za-z0-9][A-Za-z0-9_.\-]*)(?:\|[^\]]*)?\]\]").unwrap());

// [[file:path]] / [[file:path#L10]] / [[file:path#L10-20]]
static INLINE_FILE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[\[file:([^\]#|]+)(?:#[^\]|]*)?(?:\|[^\]]*)?\]\]").unwrap());

/// `---\n...\n---\n` を frontmatter と本文に分ける。
pub fn split_frontmatter(text: &str) -> Option<(&str, &str)> {
    let rest = text.strip_prefix("---\n").or_else(|| text.strip_prefix("---\r\n"))?;
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        if matches!(line, "---" | "---\n" | "---\r\n") {
            let front = rest[..offset].strip_suffix('\n').unwrap_or(&rest[..offset]);
            return Some((front.strip_suffix('\r').unwrap_or(front), &rest[offset + line.len()..]));
        }
        offset += line.len();
    }
    None
}

/// フォルダのカードの .tex を、`## Statement` / `## Proof` / `## <名前>` として本文の前に置く。
/// statement → proof → ほか（名前順）の順。card.md の本文（ノートや方針）はその後ろ。
fn attach_tex(root: &Path, dir: &Path, card: &mut Card) {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "tex"))
        .collect();
    let rank = |p: &PathBuf| match p.file_stem().and_then(|s| s.to_str()) {
        Some("statement") => 0,
        Some("proof") => 1,
        _ => 2,
    };
    files.sort_by(|a, b| (rank(a), a).cmp(&(rank(b), b)));
    let mut head = String::new();
    for f in &files {
        let (title, is_statement) = match f.file_stem().and_then(|s| s.to_str()) {
            Some("statement") => ("Statement", true),
            Some("proof") => ("Proof", false),
            Some(other) => (other, false),
            None => continue,
        };
        let text = std::fs::read_to_string(f).unwrap_or_default();
        head.push_str(&format!("## {title}\n{}\n\n", text.trim_end()));
        if is_statement {
            card.statement = Some(CardPart { text, line: 1 });
        } else if title == "Proof" {
            card.proof = Some(CardPart { text, line: 1 });
        }
        card.tex.push(f.strip_prefix(root).unwrap_or(f).to_path_buf());
    }
    card.body = head + &card.body;
}

/// TeX の数式や命令を除き、散文の文字を元のバイト位置に残す。
pub fn tex_prose_mask(input: &str, tex_comments: bool) -> String {
    term_lint::tex::prose_mask(input, tex_comments)
}

/// コードブロック（```）とインラインコード（`...`）を空白に置き換える。
/// リンクの書き方の例をリンクとして拾わないため。
pub fn strip_code(body: &str) -> String {
    mask_code(body, ' ')
}

fn mask_code(body: &str, marker: char) -> String {
    let mut markdown = String::with_capacity(body.len());
    let mut fence = None;
    for line in body.split_inclusive('\n') {
        let trimmed = line.trim_start();
        let delimiter = trimmed.chars().next().filter(|c| matches!(c, '`' | '~'));
        if let Some(c) = delimiter {
            let len = trimmed.bytes().take_while(|&byte| byte == c as u8).count();
            if len >= 3 {
                if fence.is_none() {
                    fence = Some((c, len));
                } else if fence.is_some_and(|(open, n)| c == open && len >= n)
                    && trimmed[len..].trim().is_empty()
                {
                    fence = None;
                }
                markdown.push('\n');
                continue;
            }
        }
        if fence.is_some() || trimmed.is_empty() {
            markdown.push('\n');
        } else {
            markdown.push_str(line);
        }
    }

    let mut out = String::with_capacity(markdown.len());
    // A code span can cross a soft wrap, but it ends at the paragraph boundary.
    for paragraph in markdown.split_inclusive("\n\n") {
        let mut rest = paragraph;
        while let Some(start) = rest.find('`') {
            out.push_str(&rest[..start]);
            let remaining = &rest[start..];
            let len = remaining.bytes().take_while(|&c| c == b'`').count();
            let after = &remaining[len..];
            if let Some(end) = closing_backticks(after, len) {
                out.push(marker);
                rest = &after[end..];
            } else {
                out.push_str(&remaining[..len]);
                rest = after;
            }
        }
        out.push_str(rest);
    }
    out
}

fn closing_backticks(mut text: &str, len: usize) -> Option<usize> {
    let total = text.len();
    while let Some(start) = text.find('`') {
        let run = &text[start..];
        let count = run.bytes().take_while(|&c| c == b'`').count();
        text = &run[count..];
        if count == len {
            return Some(total - text.len());
        }
    }
    None
}

pub fn parse_card(root: &Path, path: &Path, text: &str) -> Result<Card> {
    parse_card_with_tex(root, path, text, None)
}

/// Read a Markdown card and any TeX files attached to a folder card.
pub fn read_card(root: &Path, path: &Path) -> Result<Option<Card>> {
    let text = std::fs::read_to_string(path)?;
    parse_card_document(root, path, &text)
}

fn validate_card_tex(project: &Path, root: &Path, path: &Path) -> Result<()> {
    if path.ends_with("card.md") {
        for entry in std::fs::read_dir(path.parent().expect("card.md has a parent"))? {
            let path = entry?.path();
            if path.extension().is_some_and(|ext| ext == "tex") {
                workspace_paths::resolve(project, root, &path)?;
            }
        }
    }
    Ok(())
}

fn parse_card_document(root: &Path, path: &Path, text: &str) -> Result<Option<Card>> {
    if split_frontmatter(text).is_none() {
        return Ok(None);
    }
    let tex_dir = path.parent().filter(|_| path.ends_with("card.md"));
    parse_card_with_tex(root, path, text, tex_dir).map(Some)
}

fn parse_card_with_tex(root: &Path, path: &Path, text: &str, tex_dir: Option<&Path>) -> Result<Card> {
    let (front, body) = split_frontmatter(text).context("frontmatter (--- ... ---) がない")?;
    let f: Front = serde_yaml::from_str(front).context("frontmatter を読めない")?;
    let step = if f.kind.as_deref() == Some("step") {
        let mut step: step_ledger::StepMetadata =
            serde_yaml::from_str(front).context("step card metadata could not be read")?;
        step.declares_requires = f.requires.is_some();
        Some(step)
    } else {
        None
    };
    let mut links = f.links;
    links.extend(f.requires.into_iter().flatten().map(|to| Link { to, rel: "requires".into() }));
    let prose = strip_code(body);
    for cap in INLINE_CARD.captures_iter(&prose) {
        let to = cap[1].to_string();
        if !links.iter().any(|l| l.to == to) {
            links.push(Link { to, rel: "mentions".into() });
        }
    }
    let mut files = f.files;
    for cap in INLINE_FILE.captures_iter(&prose) {
        let p = cap[1].trim().to_string();
        if !files.contains(&p) {
            files.push(p);
        }
    }
    let explanation = f.kind.as_deref() == Some("explanation");
    let mut card = Card {
        id: f.id,
        external: false,
        kind: f.kind.unwrap_or_else(|| "note".into()),
        title: f.title.unwrap_or_default(),
        status: f.status.unwrap_or_default(),
        summary: f.summary.as_ref().and_then(serde_yaml::Value::as_str).map(str::to_owned),
        label: f.label,
        role: f.role,
        step,
        explains: f.explains.unwrap_or_default(),
        links,
        files,
        issues: f.issues,
        source: f.source.as_ref().and_then(source_string),
        additional_sources: f.additional_sources,
        proof_sources: f.proof_sources,
        source_sha256: f.source_sha256,
        source_id: f.source_id,
        source_fingerprint: f.source_fingerprint,
        from: f.from,
        from_thread: f.from_thread,
        lean: f.lean,
        defines: Vec::new(),
        lint_ignore: f.lint_ignore,
        tex: Vec::new(),
        path: path.strip_prefix(root).unwrap_or(path).to_path_buf(),
        body: body.to_string(),
        statement: markdown::section(text, body, "Statement"),
        proof: markdown::section(text, body, "Proof"),
        prose: explanation.then(|| markdown::prose(text, body)),
    };
    if let Some(dir) = tex_dir {
        attach_tex(root, dir, &mut card);
    }
    card.defines = match f.defines {
        Some(terms) => terms,
        None if card.kind == "definition" => term_lint::infer_terms(
            card.statement.as_ref().map(|part| part.text.as_str()),
            &prose,
        ),
        None => Vec::new(),
    };
    Ok(card)
}

fn resolve_research_origin(
    project: &Path,
    from: &str,
    cache: &mut HashMap<PathBuf, BTreeSet<String>>,
) -> Option<(PathBuf, String)> {
    let (deck, card) = from.trim().split_once('#')?;
    let path = match deck.split_once(':') {
        Some((namespace, path)) if namespace == "lab" || project.file_name().is_some_and(|n| n == namespace) => path,
        Some(_) => return None,
        None => deck,
    };
    let slug = path.strip_prefix("research/")?;
    if slug.is_empty() || slug.contains(['/', '\\']) || matches!(slug, "." | "..") {
        return None;
    }
    let deck = project.join(path).canonicalize().ok()?;
    if !deck.starts_with(project) {
        return None;
    }
    let cards = cache.entry(deck.clone()).or_insert_with(|| {
        DeckConfig::read(&deck).ok().filter(|cfg| cfg.meta.kind == "research")
            .and_then(|cfg| Graph::load_cards(&deck, &cfg.dirs).ok())
            .map(|graph| graph.cards.into_keys().collect())
            .unwrap_or_default()
    });
    cards.contains(card).then(|| (deck, card.to_string()))
}

impl Graph {
    /// デッキを開く。設定は `cards.toml`（`dirs`, `[external]`）か、
    /// 既存の論文ハーネスの `harness.toml`（`[external_harnesses]`）から読む。
    /// どちらも無ければ `cards/` だけを読む。
    pub fn open(root: &Path) -> Result<Self> {
        Self::open_with_rules(root, &[])
    }

    /// Open a deck with extra rule roots in ascending priority.
    pub fn open_with_rules(root: &Path, rules: &[PathBuf]) -> Result<Self> {
        Self::open_in_project(root, rules, None)
    }

    /// Use an explicit project root when the caller has already opened a workspace.
    pub fn open_in_project(root: &Path, rules: &[PathBuf], project: Option<&Path>) -> Result<Self> {
        let workspace = project.unwrap_or_else(|| workspace_paths::workspace_for(root));
        let cfg = DeckConfig::read_config(root, rules, Some(workspace))?;
        let mut g = Self::load_deck(root, &cfg, workspace)?;
        g.index(&cfg.lint, cfg.style, Some(workspace))?;
        Ok(g)
    }

    /// Index card metadata and cross-references without retaining local card bodies.
    pub fn open_for_browsing(root: &Path, rules: &[PathBuf], project: &Path) -> Result<Self> {
        let cfg = DeckConfig::read_config(root, rules, Some(project))?;
        let mut graph = Self::load_deck_with_bodies(root, &cfg, project, false)?;
        graph.index_links(project)?;
        Ok(graph)
    }

    /// Skip lint and serialization work if the raw card bodies alone exceed a wire budget.
    pub fn open_bounded(root: &Path, rules: &[PathBuf], project: &Path, max_body_bytes: usize) -> Result<Option<Self>> {
        let cfg = DeckConfig::read_config(root, rules, Some(project))?;
        let mut graph = Self::load_deck(root, &cfg, project)?;
        let mut remaining = max_body_bytes;
        for card in graph.cards.values() {
            let Some(next) = remaining.checked_sub(card.body.len()) else { return Ok(None); };
            remaining = next;
        }
        graph.index(&cfg.lint, cfg.style, Some(project))?;
        Ok(Some(graph))
    }

    /// Read one local card from a browsing graph, with workspace path checks.
    pub fn read_browsing_card(&self, card: &Card, project: &Path) -> Result<Card> {
        anyhow::ensure!(!card.external, "cannot reload a namespaced card as a local card");
        let path = workspace_paths::resolve(project, &self.root, &card.path)?;
        validate_card_tex(project, &self.root, &path)?;
        let loaded = read_card(&self.root, &path)?.context("Card is no longer in this deck")?;
        anyhow::ensure!(loaded.id == card.id, "Card changed while it was being read");
        Ok(loaded)
    }

    /// Complete diagnostics, including the body lint omitted from the browsing index.
    pub fn browsing_diagnostics(&self, rules: &[PathBuf], project: &Path) -> Result<Vec<Diagnostic>> {
        Ok(Self::open_in_project(&self.root, rules, Some(project))?.diagnostics)
    }

    /// Load local and namespaced cards without running the browsing lint rules.
    pub fn load_deck(root: &Path, cfg: &DeckConfig, workspace: &Path) -> Result<Self> {
        Self::load_deck_with_bodies(root, cfg, workspace, true)
    }

    fn load_deck_with_bodies(root: &Path, cfg: &DeckConfig, workspace: &Path, local: bool) -> Result<Self> {
        let mut g = Self::load_cards_with_bodies(root, &cfg.dirs, Some(workspace), local)?;
        let workspace_root = workspace.canonicalize()?;
        for (ns, resolved) in cfg.external_roots(workspace, root) {
            let ext_root = resolved?;
            let ext_root = ext_root.canonicalize()
                .with_context(|| format!("外部デッキ `{ns}` ({}) を読めない", ext_root.display()))?;
            let (scope, prefix) = if ext_root.starts_with(&workspace_root) {
                (Some(workspace), workspace_paths::relative(&g.root, &ext_root)?)
            } else {
                (None, ext_root.clone())
            };
            let ext_cfg = DeckConfig::read_config(&ext_root, &[], scope)
                .with_context(|| format!("外部デッキ `{ns}` ({}) を読めない", ext_root.display()))?;
            let ext = Self::load_cards_scoped(&ext_root, &ext_cfg.dirs, scope)?;
            for (id, mut card) in ext.cards {
                card.external = true;
                card.id = format!("{ns}:{id}");
                card.path = prefix.join(&card.path);
                for path in &mut card.tex {
                    *path = prefix.join(&path);
                }
                for link in &mut card.links {
                    if !link.to.contains(':') {
                        link.to = format!("{ns}:{}", link.to);
                    }
                }
                g.cards.insert(card.id.clone(), card);
            }
        }
        Ok(g)
    }

    /// 本文ファイルの対応を調べるため、ローカルカードだけを読む。
    pub fn open_local(root: &Path) -> Result<Self> {
        Self::load_cards(root, &DeckConfig::read(root)?.dirs)
    }

    /// Load only a deck's own cards with the workspace path boundary.
    pub fn open_local_in_workspace(root: &Path, workspace: &Path) -> Result<Self> {
        let cfg = DeckConfig::read_in_workspace(root, workspace)?;
        Self::load_cards_scoped(root, &cfg.dirs, Some(workspace))
    }

    /// Check the configured style rules on cards already loaded by the caller.
    pub fn lint_style_subset(root: &Path, rules: &[PathBuf], cards: BTreeMap<String, Card>, project: Option<&Path>) -> Result<Vec<Diagnostic>> {
        let cfg = DeckConfig::read_with_rules(root, rules)?;
        let graph = Self { root: root.to_path_buf(), cards, ..Self::default() };
        style_lint::check(&graph, cfg.style, project, |_| true)
    }

    /// 指定されたカードの用語・文体を検査する。定義と依存はデッキ全体から解決する。
    pub fn lint_cards(root: &Path, ids: &BTreeSet<String>) -> Result<Vec<Diagnostic>> {
        Self::lint_cards_with_rules(root, ids, &[])
    }

    pub fn lint_cards_with_rules(root: &Path, ids: &BTreeSet<String>, rules: &[PathBuf]) -> Result<Vec<Diagnostic>> {
        let workspace = workspace_paths::workspace_for(root);
        let cfg = DeckConfig::read_config(root, rules, Some(workspace))?;
        let graph = Self::load_deck(root, &cfg, workspace)?;
        graph.lint(&cfg.lint, cfg.style, None, |card| ids.contains(&card.id))
    }

    /// `root` を根に、`card_dirs` 以下の `*.md` を全部読む（外部デッキなし）。
    /// デッキの設定が読める場合は、lint の設定も使う。
    pub fn load(root: &Path, card_dirs: &[PathBuf]) -> Result<Self> {
        Self::load_with_rules(root, card_dirs, &[])
    }

    pub fn load_with_rules(root: &Path, card_dirs: &[PathBuf], rules: &[PathBuf]) -> Result<Self> {
        let mut g = Self::load_cards(root, card_dirs)?;
        let (lint, style) = match DeckConfig::read_with_rules(root, rules) {
            Ok(cfg) => (cfg.lint, cfg.style),
            Err(_) if rules.is_empty() => Default::default(),
            Err(e) => return Err(e),
        };
        g.index(&lint, style, None)?;
        Ok(g)
    }

    fn load_cards(root: &Path, card_dirs: &[PathBuf]) -> Result<Self> {
        Self::load_cards_scoped(root, card_dirs, None)
    }

    fn load_cards_scoped(root: &Path, card_dirs: &[PathBuf], workspace: Option<&Path>) -> Result<Self> {
        Self::load_cards_with_bodies(root, card_dirs, workspace, true)
    }

    fn load_cards_with_bodies(root: &Path, card_dirs: &[PathBuf], workspace: Option<&Path>, retain_bodies: bool) -> Result<Self> {
        let root = root.canonicalize().with_context(|| format!("{} がない", root.display()))?;
        let mut g = Graph { root: root.clone(), ..Default::default() };
        let mut seen = BTreeSet::new();
        for dir in card_dirs {
            if let Some(workspace) = workspace {
                workspace_paths::resolve(workspace, &root, dir)?;
            }
            let dir = root.join(dir);
            if !dir.exists() {
                continue; // カードのないプロジェクトは 0 枚として扱う
            }
            for entry in walkdir::WalkDir::new(&dir).sort_by_file_name().into_iter()
                .filter_entry(|entry| !fuzukue_proto::is_thread_save_artifact(entry.path()))
            {
                let entry = entry?;
                let p = entry.path();
                if !entry.file_type().is_file() || p.extension().is_none_or(|e| e != "md") {
                    continue;
                }
                // Configured roots can overlap or name the same directory through a symlink.
                if !seen.insert(p.canonicalize()?) {
                    continue;
                }
                if let Some(workspace) = workspace {
                    validate_card_tex(workspace, &root, p)?;
                }
                let rel = p.strip_prefix(&root).unwrap_or(p).to_path_buf();
                let parsed = parse_card_document(&root, p, &std::fs::read_to_string(p)?);
                match parsed {
                    Ok(None) => continue, // README などカードでない Markdown
                    Ok(Some(mut card)) => {
                        if !retain_bodies {
                            card.body = String::new();
                            card.statement = None;
                            card.proof = None;
                            card.prose = None;
                        }
                        if let Some(prev) = g.cards.get(&card.id) {
                            g.diag(Severity::Error, &rel, format!("id `{}` が {} と重複", card.id, prev.path.display()));
                        } else {
                            g.cards.insert(card.id.clone(), card);
                        }
                    }
                    Err(e) => g.diag(Severity::Error, &rel, format!("{e:#}")),
                }
            }
        }
        Ok(g)
    }

    fn diag(&mut self, severity: Severity, path: &Path, message: String) {
        self.diagnostics.push(Diagnostic::new(severity, None, path.to_path_buf(), message));
    }

    fn index(&mut self, lint: &LintConfig, style: StyleConfig, project: Option<&Path>) -> Result<()> {
        let project = project.map(Path::to_path_buf).unwrap_or_else(|| {
            self.root.ancestors().find(|p| fuzukue_proto::compat::workspace_config(p).is_file())
                .or_else(|| self.root.ancestors().find(|p| p.join("research").is_dir()))
                .unwrap_or(&self.root).to_path_buf()
        });
        let project = project.canonicalize()?;
        self.index_links(&project)?;
        let diagnostics = self.lint(lint, style, Some(&project), |_| true)?;
        self.diagnostics.extend(diagnostics);
        self.diagnostics.sort_by(|a, b| (&a.severity, &a.path).cmp(&(&b.severity, &b.path)));
        Ok(())
    }

    fn index_links(&mut self, project: &Path) -> Result<()> {
        let project = project.canonicalize()?;
        let mut problems = Vec::new();
        let mut research = HashMap::new();
        for card in self.cards.values() {
            if card.external {
                continue;
            }
            if let Some(from) = &card.from {
                if let Some(origin) = resolve_research_origin(&project, from, &mut research) {
                    self.research_origins.insert(card.id.clone(), origin);
                } else {
                    problems.push((Severity::Warning, card.path.clone(), format!("研究の出どころ `{from}` が見つからない")));
                }
            }
            for l in &card.links {
                if self.cards.contains_key(&l.to) {
                    self.backlinks.entry(l.to.clone()).or_default().push((card.id.clone(), l.rel.clone()));
                } else {
                    problems.push((Severity::Error, card.path.clone(), format!("リンク先のカード `{}` がない ({})", l.to, l.rel)));
                }
            }
            for f in &card.files {
                let norm = f.trim_end_matches('/').to_string();
                if !self.root.join(&norm).exists() {
                    problems.push((Severity::Warning, card.path.clone(), format!("ファイル `{f}` がない")));
                }
                self.file_refs.entry(norm).or_default().insert(card.id.clone());
            }
        }
        for (s, p, m) in problems {
            self.diag(s, &p, m);
        }
        Ok(())
    }

    fn lint(
        &self,
        lint: &LintConfig,
        style: StyleConfig,
        project: Option<&Path>,
        includes: impl Fn(&Card) -> bool,
    ) -> Result<Vec<Diagnostic>> {
        let mut diagnostics = term_lint::check(self, lint, project, &includes)?;
        diagnostics.extend(style_lint::check(self, style, project, includes)?);
        Ok(diagnostics)
    }

    /// ファイル（またはその親ディレクトリ）を指しているカード。
    pub fn cards_for_file(&self, rel: &str) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        let mut p = Some(Path::new(rel.trim_end_matches('/')));
        while let Some(cur) = p {
            if let Some(ids) = self.file_refs.get(cur.to_string_lossy().as_ref()) {
                out.extend(ids.iter().cloned());
            }
            p = cur.parent().filter(|x| !x.as_os_str().is_empty());
        }
        out
    }

    /// 入口カードから `FOLLOW_RELS` を辿った閉包を、依存が先に来る順で返す。
    /// 循環があっても止まる（初めて訪れた順で切る）。
    pub fn context(&self, entries: &[String]) -> Vec<String> {
        let mut order = Vec::new();
        let mut seen = BTreeSet::new();
        fn visit(g: &Graph, id: &str, seen: &mut BTreeSet<String>, order: &mut Vec<String>) {
            if !seen.insert(id.to_string()) {
                return;
            }
            if let Some(card) = g.cards.get(id) {
                for l in &card.links {
                    if FOLLOW_RELS.contains(&l.rel.as_str()) {
                        visit(g, &l.to, seen, order);
                    }
                }
                order.push(id.to_string());
            }
        }
        for e in entries {
            visit(self, e, &mut seen, &mut order);
        }
        order
    }

    pub fn has_errors(&self) -> bool {
        self.diagnostics.iter().any(|d| d.severity == Severity::Error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, rel: &str, text: &str) {
        let p = dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }

    #[test]
    fn parses_paper_provenance_and_warns_for_unknown_research_cards() {
        let project = std::env::temp_dir().join(format!("fuzukue-card-provenance-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&project);
        write(&project, "fuzukue.toml", "");
        write(&project, "research/metric/deck.toml", "kind = 'research'\n");
        write(&project, "research/metric/cards/q.md", "---\nid: q\nkind: question\n---\nQuestion\n");
        let paper = project.join("papers/p");
        write(&paper, "deck.toml", "kind = 'paper'\n");
        for (id, origin) in [
            ("qualified", "lab:research/metric#q"), ("local", "research/metric#q"),
            ("missing", "research/metric#missing"),
        ] {
            write(&paper, &format!("cards/{id}.md"), &format!("---\nid: {id}\nfrom: {origin}\n---\n"));
        }
        let graph = Graph::open(&paper).unwrap();
        assert_eq!(graph.cards["qualified"].from.as_deref(), Some("lab:research/metric#q"));
        assert_eq!(graph.cards["local"].from.as_deref(), Some("research/metric#q"));
        assert_eq!(graph.research_origins["qualified"], graph.research_origins["local"]);
        assert_eq!(graph.research_origins["local"].1, "q");
        assert!(!graph.research_origins.contains_key("missing"));
        assert_eq!(graph.diagnostics.len(), 1);
        assert_eq!(graph.diagnostics[0].severity, Severity::Warning);
        assert!(graph.diagnostics[0].message.contains("research/metric#missing"));
        let _ = std::fs::remove_dir_all(project);
    }

    #[test]
    fn finds_harness_and_legacy_decks_once() {
        let dir = std::env::temp_dir().join(format!("fuzukue-cards-decks-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        write(&dir, "papers/a/deck.toml", "");
        write(&dir, "writing/b/harness/harness.toml", "");
        write(&dir, "old/c/cards.toml", "");
        write(&dir, "both/deck.toml", "");
        write(&dir, "both/harness.toml", "");
        write(&dir, "target/x/deck.toml", "");
        let found: Vec<_> = find_decks(&dir).into_iter().map(|p| p.strip_prefix(&dir).unwrap().to_path_buf()).collect();
        assert_eq!(found, ["both", "old/c", "papers/a", "writing/b/harness"].map(PathBuf::from));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn fixture() -> (PathBuf, Graph) {
        // テストは並んで走るので、呼ぶたびに別のディレクトリを使う。
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("fuzukue-cards-test-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        write(&dir, "src/a.rs", "");
        write(&dir, "cards/a.md", "---\nid: a\nkind: spec\nrequires: [b]\nfiles: [src/a.rs]\n---\nsee [[c]] and [[file:src/a.rs#L3]]\n");
        write(&dir, "cards/b.md", "---\nid: b\nlinks:\n  - {to: a, rel: refines}\n---\n");
        write(&dir, "cards/c.md", "---\nid: c\nlinks:\n  - {to: missing, rel: requires}\nfiles: [nope.rs]\n---\n");
        write(&dir, "cards/README.md", "# not a card\n");
        let g = Graph::load(&dir, &[PathBuf::from("cards")]).unwrap();
        (dir, g)
    }

    #[test]
    fn loads_and_indexes() {
        let (_d, g) = fixture();
        assert_eq!(g.cards.len(), 3);
        let a = &g.cards["a"];
        assert!(a.links.contains(&Link { to: "b".into(), rel: "requires".into() }));
        assert!(a.links.contains(&Link { to: "c".into(), rel: "mentions".into() }));
        assert_eq!(g.backlinks["a"], vec![("b".to_string(), "refines".to_string())]);
        assert_eq!(g.cards_for_file("src/a.rs"), BTreeSet::from(["a".to_string()]));
    }

    #[test]
    fn macros_accept_strings_and_arrays() {
        let dir = std::env::temp_dir().join(format!("fuzukue-cards-macros-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        write(&dir, "harness.toml", "[mathjax_macros]\nGm = [\"\\\\Gamma_{#1}\", 1]\nR = \"\\\\mathbb{R}\"\nZ = [\"\\\\mathbb{Z}\", 0]\n");
        let cfg = DeckConfig::read(&dir).unwrap();
        assert_eq!(cfg.meta.macros.get("R").map(String::as_str), Some("\\mathbb{R}"));
        assert_eq!(cfg.meta.macros.get("Z").map(String::as_str), Some("\\mathbb{Z}"));
        assert_eq!(cfg.meta.macros_with_args.get("Gm"), Some(&("\\Gamma_{#1}".to_string(), 1)));
    }

    #[test]
    fn legacy_harness_is_deck_relative_and_manifests_are_selected_in_order() -> Result<()> {
        let workspace = tempfile::tempdir()?;
        let root = workspace.path();
        let deck = root.join("papers/first");
        write(&deck, "harness.toml", "title = 'Harness'\nbibliography = ''\nhtml_out = '../../site/first'\n[external_harnesses]\nother = '../second'\n");
        write(root, "papers/second/cards/base.md", "---\nid: base\nkind: definition\n---\n");
        let harness = DeckConfig::read(&deck)?;
        assert!(harness.harness);
        assert!(harness.meta.bibliography.is_none());
        assert_eq!(harness.html_dir(root, &deck)?, root.canonicalize()?.join("site/first"));
        assert!(Graph::open_in_project(&deck, &[], Some(root))?.cards.contains_key("other:base"));

        write(&deck, "cards.toml", "title = 'Cards'\n");
        let cards = DeckConfig::read(&deck)?;
        assert!(!cards.harness);
        assert_eq!(cards.meta.title, "Cards");
        assert!(cards.html.html_out.is_none());
        assert!(cards.external_roots(root, &deck).next().is_none());

        write(&deck, "deck.toml", "title = 'Deck'\n");
        assert_eq!(DeckConfig::read(&deck)?.meta.title, "Deck");
        Ok(())
    }

    #[test]
    fn folder_cards_read_tex_files() {
        let dir = std::env::temp_dir().join(format!("fuzukue-cards-folder-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        write(&dir, "cards/thm/x/card.md", "---\nid: x\nkind: theorem\nlean: [Pkg.x]\n---\n## Notes\nmemo\n");
        write(&dir, "cards/thm/x/proof.tex", "P");
        write(&dir, "cards/thm/x/statement.tex", "S");
        let g = Graph::load(&dir, &[PathBuf::from("cards")]).unwrap();
        let c = &g.cards["x"];
        assert_eq!(c.body, "## Statement\nS\n\n## Proof\nP\n\n## Notes\nmemo\n");
        assert_eq!(c.tex.len(), 2);
        assert_eq!(c.lean, vec!["Pkg.x".to_string()]);
    }

    #[test]
    fn links_in_code_are_ignored() {
        let card = parse_card(Path::new("/"), Path::new("/x.md"),
            "---\nid: x\n---\nreal [[a]], `[[b]]`\n```\n[[c]] [[file:d]]\n```\n").unwrap();
        let tos: Vec<_> = card.links.iter().map(|l| l.to.as_str()).collect();
        assert_eq!(tos, vec!["a"]);
        assert!(card.files.is_empty());
    }

    #[test]
    fn diagnostics() {
        let (_d, g) = fixture();
        assert!(g.has_errors());
        let msgs: Vec<_> = g.diagnostics.iter().map(|d| d.message.as_str()).collect();
        assert!(msgs.iter().any(|m| m.contains("`missing`")));
        assert!(msgs.iter().any(|m| m.contains("`nope.rs`")));
    }

    #[test]
    fn context_follows_requires_not_mentions() {
        let (_d, g) = fixture();
        // a requires b, b refines a（循環）。c は mentions なので入らない。
        assert_eq!(g.context(&["a".into()]), vec!["b".to_string(), "a".to_string()]);
    }
}
