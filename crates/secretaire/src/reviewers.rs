//! 独立した査読と、査読結果からコメント下書きへの変換。

use std::collections::{BTreeMap, BTreeSet};
use std::io::ErrorKind;
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, ensure, Context, Result};
use fuzukue_cards::{Card, Graph};
use fuzukue_proto::{AgentBackend, AgentInfo, AgentSelection, CommentView, DocItem, ReviewSettings, ServerEvent};
use serde::{Deserialize, Serialize};

use crate::cancellation::{Cancellation, Cancelled};
use crate::{cards, comments, safe_join, usage, workspace::resolve_deck, Workspace};

pub(crate) mod section;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Kind {
    Style,
    Lean,
    Math,
    Output,
    /// 人の編集に合わせて、影響を受ける所の整合を取る案を出す。人の編集が閉じた時だけ動く。
    Consistency,
}

impl Kind {
    fn parse(kind: &str) -> Result<Self> {
        match kind {
            "style" => Ok(Self::Style),
            "lean" => Ok(Self::Lean),
            "math" => Ok(Self::Math),
            "output" => Ok(Self::Output),
            "consistency" => Ok(Self::Consistency),
            _ => bail!("査読の kind は style、lean、math、consistency のいずれかで指定する"),
        }
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Style => "style",
            Self::Lean => "lean",
            Self::Math => "math",
            Self::Output => "output",
            Self::Consistency => "consistency",
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Style => "[文体査読]",
            Self::Lean => "[Lean 査読]",
            Self::Math => "[数学査読]",
            Self::Output => "[出力査読]",
            Self::Consistency => "[整合]",
        }
    }

    pub(crate) const ALL: [Self; 5] = [Self::Style, Self::Lean, Self::Math, Self::Output, Self::Consistency];

    pub(crate) fn agent_name(self) -> String {
        format!("review-{}", self.name())
    }

    pub(crate) fn selection(self, settings: &ReviewSettings) -> AgentSelection {
        match self {
            Self::Style => AgentSelection::from_model(settings.style_model.clone(), settings.style_effort.clone()),
            Self::Lean | Self::Math | Self::Output | Self::Consistency => AgentSelection::from_model(Some(settings.math_model.clone()),
                (!settings.math_effort.is_empty()).then(|| settings.math_effort.clone())),
        }
    }

    pub(crate) fn agent_info(self, project: &Path, rules: &crate::rules::Rules) -> Result<AgentInfo> {
        let description = match self {
            Self::Style => harness_text!("Review readability and writing style", "文章の読みやすさと文体を査読する"),
            Self::Lean => harness_text!("Check Lean declarations against the manuscript’s mathematical claims", "Lean 宣言と原稿の数学的主張の対応を査読する"),
            Self::Math => harness_text!("Review mathematical assumptions and proofs for correctness", "仮定と証明の数学的な正しさを査読する"),
            Self::Output => harness_text!("Review the rendered section and its document context", "描画された節と文書内での表示を査読する"),
            Self::Consistency => harness_text!("Propose follow-up fixes elsewhere so the manuscript stays consistent with a human edit", "人の編集に合わせて、ほかの所の整合を取る直し案を出す"),
        };
        let selection = self.selection(&ReviewSettings::default());
        Ok(AgentInfo {
            name: self.agent_name(), description: description.into(), backend: selection.backend,
            model: selection.model.unwrap_or_default(), effort: selection.effort, tools: Vec::new(),
            prompt: self.instructions(project, rules)?, path: None, tier: None,
            policy: Default::default(),
        })
    }

    fn instructions(self, project: &Path, rules: &crate::rules::Rules) -> Result<String> {
        Ok(rules.reviewer(project, &format!("{}.md", self.name()))?.unwrap_or_else(|| match self {
            Self::Style => harness_text!(include_str!("../../../reviewers/style.md"), include_str!("../prompts/ja/reviewers/style.md")).into(),
            Self::Lean => harness_text!(include_str!("../../../reviewers/lean.md"), include_str!("../prompts/ja/reviewers/lean.md")).into(),
            Self::Math => harness_text!(include_str!("../../../reviewers/math.md"), include_str!("../prompts/ja/reviewers/math.md")).into(),
            Self::Output => harness_text!(
                "Review the captured document output for missing section content, broken references, indexing and numbering, and rendering problems. Use the build diagnostics and verify against the supplied PDF and source.",
                "固定した文書出力について、節の内容の欠落、参照、索引、番号付け、描画の問題を調べる。ビルド診断を手掛かりに、渡された PDF と原文で確かめる。",
            ).into(),
            Self::Consistency => harness_text!(include_str!("../../../reviewers/consistency.md"), include_str!("../prompts/ja/reviewers/consistency.md")).into(),
        }))
    }
}

#[derive(Serialize)]
struct ReviewCard {
    id: String,
    notes: PathBuf,
    tex: Vec<PathBuf>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    lean: Vec<String>,
}

impl ReviewCard {
    fn new(card: &Card, deck: &Path, kind: Kind) -> Self {
        let notes = deck.join(&card.path);
        // フォルダの TeX は card.md と同じ場所にある。外部カードでも notes は
        // 取り込み先のデッキからのパスに解決済み。
        let tex = card
            .tex
            .iter()
            .filter_map(|path| Some(notes.parent()?.join(path.file_name()?)))
            .collect();
        let lean = if kind == Kind::Lean {
            card.lean.clone()
        } else {
            Vec::new()
        };
        Self {
            id: card.id.clone(),
            notes,
            tex,
            lean,
        }
    }
}

#[derive(Serialize)]
pub(crate) struct EditedPassage {
    id: String,
    path: PathBuf,
    edited: RangeInclusive<usize>,
    excerpt: Vec<(usize, String)>,
}

impl EditedPassage {
    pub(crate) fn new(
        id: &str,
        path: PathBuf,
        lines: &[&str],
        edited: RangeInclusive<usize>,
        file_offset: usize,
    ) -> Self {
        const CONTEXT: usize = 2;
        let first = (*edited.start()).saturating_sub(CONTEXT).max(1);
        let last = (*edited.end() + CONTEXT).min(lines.len());
        Self {
            id: id.into(),
            path,
            edited: edited.start() + file_offset..=edited.end() + file_offset,
            excerpt: (first..=last)
                .map(|line| (line + file_offset, lines[line - 1].into()))
                .collect(),
        }
    }

    fn contains(&self, finding: &Finding) -> bool {
        let Some((location, comment)) = finding.text.split_once('\n') else {
            return false;
        };
        let Some((path, lines)) = location.trim().rsplit_once(":L") else {
            return false;
        };
        let Some((start, end)) = lines.split_once("-L") else {
            return false;
        };
        let (Ok(start), Ok(end)) = (start.parse::<usize>(), end.parse::<usize>()) else {
            return false;
        };
        self.id == finding.card
            && self.path == Path::new(path)
            && start <= end
            && self.edited.contains(&start)
            && self.edited.contains(&end)
            && !comment.trim().is_empty()
    }
}

pub(crate) struct Review {
    kind: Kind,
    instructions: String,
    cards: Vec<ReviewCard>,
    scoped: bool,
    passages: Vec<EditedPassage>,
    captured: Option<serde_json::Value>,
    /// 整合の確認では、サーバーが組み立てた本文と、指摘してよいカードを持つ。
    custom: Option<(String, BTreeSet<String>)>,
}

impl Review {
    /// 人の編集に合わせた整合の確認。`body` は編集と候補を説明する本文、`allowed` は指摘してよいカード。
    pub(crate) fn consistency(
        project: &Path,
        body: String,
        allowed: BTreeSet<String>,
        rules: &crate::rules::Rules,
    ) -> Result<Self> {
        Ok(Self {
            kind: Kind::Consistency,
            instructions: Kind::Consistency.instructions(project, rules)?,
            cards: Vec::new(),
            scoped: true,
            passages: Vec::new(),
            captured: None,
            custom: Some((body, allowed)),
        })
    }

    pub(crate) fn prepare(project: &Path, root: &Path, kind: Kind) -> Result<Self> {
        let deck = root.strip_prefix(project)?;
        let rules = crate::rules::resolve(project)?;
        let graph = Graph::open_with_rules(root, &rules.layers(project))?;
        let sections = match cards::read_structure(root) {
            Ok((_, sections)) => sections,
            Err(e)
                if kind == Kind::Lean
                    && e.downcast_ref::<std::io::Error>()
                        .is_some_and(|e| e.kind() == ErrorKind::NotFound) =>
            {
                Vec::new()
            }
            Err(e) => return Err(e).context("査読対象の structure.yaml を読めない"),
        };
        let mut ordered = Vec::new();
        for item in sections.into_iter().flat_map(|sec| sec.items) {
            if let DocItem::Card { id } = item {
                let card = graph
                    .cards
                    .get(&id)
                    .with_context(|| format!("structure.yaml のカード `{id}` がない"))?;
                ordered.push(ReviewCard::new(card, deck, kind));
            }
        }
        if kind == Kind::Lean {
            let listed: BTreeSet<_> = ordered.iter().map(|c| c.id.clone()).collect();
            ordered.extend(
                graph
                    .cards
                    .values()
                    .filter(|c| !c.external && !c.lean.is_empty() && !listed.contains(&c.id))
                    .map(|c| ReviewCard::new(c, deck, kind)),
            );
        }
        ensure!(!ordered.is_empty(), "査読対象のカードがない");
        Ok(Self {
            kind,
            instructions: kind.instructions(project, &rules)?,
            cards: ordered,
            scoped: false,
            passages: Vec::new(),
            captured: None,
            custom: None,
        })
    }

    /// 変更カードの一覧から査読対象を作る。構成に未掲載のカードも対象にできる。
    pub(crate) fn scoped<'a>(
        project: &Path,
        deck: &Path,
        cards: impl IntoIterator<Item = &'a Card>,
        kind: Kind,
        rules: &crate::rules::Rules,
    ) -> Result<Option<Self>> {
        let cards: Vec<_> = cards
            .into_iter()
            .filter(|card| !card.external && (kind != Kind::Lean || !card.lean.is_empty()))
            .map(|card| ReviewCard::new(card, deck, kind))
            .collect();
        if cards.is_empty() {
            return Ok(None);
        }
        Ok(Some(Self {
            kind,
            instructions: kind.instructions(project, rules)?,
            cards,
            scoped: true,
            passages: Vec::new(),
            captured: None,
            custom: None,
        }))
    }

    pub(crate) fn edited(
        project: &Path,
        passages: Vec<EditedPassage>,
        rules: &crate::rules::Rules,
    ) -> Result<Option<Self>> {
        if passages.is_empty() {
            return Ok(None);
        }
        Ok(Some(Self {
            kind: Kind::Style,
            instructions: Kind::Style.instructions(project, rules)?,
            cards: Vec::new(),
            scoped: true,
            passages,
            captured: None,
            custom: None,
        }))
    }

    pub(crate) fn prompt(&self, deck: &str, lean_status: &[(String, String)]) -> Result<String> {
        let deck = if deck.is_empty() { "." } else { deck };
        if let Some((body, _)) = &self.custom {
            return Ok(harness_format!(
                "{}\n\nDeck: {} (project-relative)\n{}", "{}\n\nデッキ: {}（プロジェクト相対）\n{}",
                format!("{}{}", self.instructions, crate::rules::conversation_voice()),
                serde_json::to_string(deck)?,
                body,
            ));
        }
        if !self.passages.is_empty() {
            return Ok(harness_format!(
                "{}\n\nDeck: {} (project-relative)\nEdits (path is project-relative, excerpt entries are [line number, source text]):\n{}\n{}", "{}\n\nデッキ: {}（プロジェクト相対）\n編集箇所（path はプロジェクト相対、excerpt は [行番号, 原文] の組）:\n{}\n{}",
                format!("{}{}", self.instructions, crate::rules::conversation_voice()),
                serde_json::to_string(deck)?,
                serde_json::to_string_pretty(&self.passages)?,
                harness_text!(
                    "\nFor automated style review, read only the edits listed above. This scope takes precedence over instructions to read the full document in publication order.\nIn each excerpt, report findings from edited.start through edited.end. Use surrounding lines for context only. Keep findings inside the edited range.\nDo not edit files or comments, write through external tools, or delegate changes.\nReturn one JSON object only. Start each finding’s text with path:L<start>-L<end>, using lines within one edited range. Give both endpoints even for a single line (for example, L5-L5).\n{\"findings\":[{\"card\":\"<id>\",\"text\":\"<path>:L5-L6\\n<exact quote, issue, concrete revision>\"}]}\nIf there are no findings, return {\"findings\":[]}.\n",
                    concat!(
                        "\nこの自動文体査読では、上の編集箇所だけを読む。掲載順に全体を読む指示より、この範囲指定を優先する。\n",
                        "excerpt のうち edited の start から end までが指摘対象。前後の行は文脈を確認するために使う。対象外の行を指摘に含めない。\n",
                        "ファイルとコメントを編集せず、外部ツールでの書き込みや変更作業の委任も行わない。\n",
                        "最終回答は JSON オブジェクト一つだけにする。各指摘の text の一行目は path:L開始行-L終了行 の形で、",
                        "一つの edited に収まる問題箇所の行を指定する。同じ行でも L5-L5 のように両端を書く。\n",
                        r#"{"findings":[{"card":"<id>","text":"<path>:L5-L6\n<原文の引用、指摘、具体的な修正案>"}]}"#,
                        "\n指摘がなければ {\"findings\":[]} を返す。\n",
                    ),
                ),
            ));
        }
        let order = if self.captured.is_some() {
            harness_text!("section order", "節内の順序")
        } else if self.scoped {
            harness_text!("changed cards only", "変更カードのみ")
        } else {
            harness_text!("publication order", "掲載順")
        };
        let mut prompt = harness_format!(
            "{}\n\nDeck: {} (project-relative)\nCards ({order}, with project-relative paths):\n{}\n", "{}\n\nデッキ: {}（プロジェクト相対）\nカードの一覧（{order}、パスはプロジェクト相対）:\n{}\n",
            format!("{}{}", self.instructions, crate::rules::conversation_voice()),
            serde_json::to_string(deck)?,
            serde_json::to_string_pretty(&self.cards)?,
        );
        if self.scoped {
            prompt.push_str(harness_text!("\nReport findings only for these cards. Read dependencies to verify evidence if needed.\n", "\n指摘の対象はこの一覧のカードに限る。依存先は根拠を確認するために参照できる。\n"));
        }
        if let Some(captured) = &self.captured {
            prompt.push_str(harness_text!(
                "\nYour working directory is the capture made for this review. Use only the captured files listed below. Treat missing evidence as unchecked.\n",
                "\n作業ディレクトリは依頼時の写し。下記に挙げたファイルだけを根拠に使い、根拠のない項目は未確認とする。\n",
            ));
            prompt.push_str(&serde_json::to_string_pretty(captured)?);
            prompt.push('\n');
        }
        if self.kind == Kind::Lean {
            let cached: BTreeMap<_, _> = lean_status
                .iter()
                .map(|(decl, state)| (decl.as_str(), state.as_str()))
                .collect();
            let statuses: BTreeMap<_, _> = self
                .cards
                .iter()
                .flat_map(|card| &card.lean)
                .map(|decl| {
                    (
                        decl,
                        cached.get(decl.as_str()).copied().unwrap_or("unchecked"),
                    )
                })
                .collect();
            prompt.push_str(harness_text!("\nLean status (latest server check, unchecked means not verified):\n", "\nLean の状態（サーバーの直近の検査。unchecked は未確認）:\n"));
            if let Some(error) = cached.get("(lean)") {
                prompt.push_str(&harness_format!("Overall check status: {error}\n", "検査全体の状態: {error}\n"));
            }
            prompt.push_str(&serde_json::to_string_pretty(&statuses)?);
            prompt.push('\n');
        }
        prompt.push_str(harness_text!(
            "\nRead the source at the paths above. Do not edit files or comments, write through external tools, or delegate changes.\nReturn one JSON object only, without a preamble or code fence.\n{\"findings\":[{\"card\":\"<card ID>\",\"text\":\"<issue, exact supporting quote, concrete revision>\"}]}\nUse IDs from the card list. If there are no findings, return {\"findings\":[]}.\n",
            concat!(
                "\n上のパスから原文を読み、ファイルやコメントを編集しない。外部ツールでの書き込みや変更作業の委任も行わない。\n",
                "最終回答は JSON オブジェクト一つだけにする。前置きやコードフェンスを付けない。\n",
                "{\"findings\":[{\"card\":\"<カードID>\",\"text\":\"<指摘、根拠の引用、具体的な修正案>\"}]}\n",
                "card にはカード一覧の ID を使う。指摘がなければ {\"findings\":[]} を返す。\n",
            ),
        ));
        Ok(prompt)
    }

    /// 文体査読は編集範囲、Lean 査読は変更カードの一覧で照合する。
    pub(crate) fn findings(&self, answer: &str) -> Result<Vec<Finding>> {
        let mut findings = parse_findings(answer)?;
        findings.retain(|finding| {
            if let Some((_, allowed)) = &self.custom {
                allowed.contains(&finding.card)
            } else if self.passages.is_empty() {
                self.cards.iter().any(|card| card.id == finding.card)
            } else {
                self.passages
                    .iter()
                    .any(|passage| passage.contains(finding))
            }
        });
        Ok(findings)
    }

    pub(crate) fn append_drafts(&self, root: &Path, answer: &str) -> Result<DraftUpdate> {
        // 査読中に削除されたカードへの下書きも除外する。
        let graph = Graph::open(root)?;
        let mut items = comments::read(root)?;
        let old_len = items.len();
        let created = usage::now();
        let mut append = |card: &str, text: &str| {
            items.push(CommentView {
                card: card.into(),
                quote: None,
                proof: None,
                source: None,
                text: format!("{} {text}", self.kind.label()),
                created,
            });
        };
        let first = || {
            self.cards
                .iter()
                .map(|c| c.id.as_str())
                .find(|id| graph.cards.contains_key(*id))
                .or_else(|| {
                    graph
                        .cards
                        .values()
                        .find(|c| !c.external)
                        .map(|c| c.id.as_str())
                })
                .context("査読結果を置くカードがない")
        };
        let result = match parse_findings(answer) {
            Ok(findings) => {
                let mut unknown = BTreeSet::new();
                for finding in findings {
                    if graph.cards.contains_key(&finding.card) {
                        append(&finding.card, &finding.text);
                    } else {
                        unknown.insert(finding.card);
                    }
                }
                if !unknown.is_empty() {
                    let ids = serde_json::to_string(&unknown)?;
                    append(
                        first()?,
                        &format!("デッキにないカードへの指摘を除外した。カード ID: {ids}"),
                    );
                }
                Ok(items.len() - old_len)
            }
            Err(error) => {
                let tail_start = answer.char_indices().rev().nth(1999).map_or(0, |(i, _)| i);
                append(
                    first()?,
                    &format!(
                        "査読結果を JSON として読めなかった（{error}）。返答の末尾:\n{}",
                        &answer[tail_start..]
                    ),
                );
                Err(error)
            }
        };
        if items.len() != old_len {
            comments::save(root, &items)?;
        }
        Ok(DraftUpdate { items, result })
    }
}

pub(crate) struct DraftUpdate {
    pub(crate) items: Vec<CommentView>,
    pub(crate) result: serde_json::Result<usize>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct Finding {
    pub(crate) card: String,
    pub(crate) text: String,
}

pub(crate) fn parse_findings(answer: &str) -> serde_json::Result<Vec<Finding>> {
    #[derive(Deserialize)]
    struct ReviewAnswer {
        findings: Vec<Finding>,
    }

    let text = answer.trim();
    let json = text
        .split_once('\n')
        .filter(|(opening, _)| matches!(opening.trim_end(), "```json" | "```"))
        .and_then(|(_, body)| body.strip_suffix("\n```"))
        .unwrap_or(text);
    let tidy = |mut findings: Vec<Finding>| {
        for f in &mut findings {
            f.text = collapse_double_backslash(&f.text);
        }
        findings
    };
    match serde_json::from_str::<ReviewAnswer>(json).map(|answer| tidy(answer.findings)) {
        Ok(findings) => Ok(findings),
        // TeX の `\alpha` などをエスケープせずに書いてくることがある。読み直す。
        Err(e) => serde_json::from_str::<ReviewAnswer>(&escape_stray_backslashes(json))
            .map(|answer| tidy(answer.findings))
            .map_err(|_| e),
    }
}

/// 二重にエスケープされた TeX（`\\exp`）を `\exp` に戻す。英字の前の `\\` だけを見る。
fn collapse_double_backslash(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find("\\\\") {
        out.push_str(&rest[..i]);
        let after = &rest[i + 2..];
        if after.chars().next().is_some_and(|c| c.is_ascii_alphabetic()) {
            out.push('\\');
        } else {
            out.push_str("\\\\");
        }
        rest = after;
    }
    out.push_str(rest);
    out
}

/// 査読の返事の形。Codex の turn/start に outputSchema として渡す。
pub(crate) fn answer_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "findings": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": { "card": { "type": "string" }, "text": { "type": "string" } },
                    "required": ["card", "text"],
                    "additionalProperties": false
                }
            }
        },
        "required": ["findings"],
        "additionalProperties": false
    })
}

/// JSON 文字列の中の、JSON のエスケープとして読めない（または TeX のコマンドに見える）`\` を `\\` にする。
/// `\n` `\t` などは、後ろに英字が続く（`\nabla` `\theta` `\beta` `\frac` `\rho`）ときだけ TeX とみなす。
fn escape_stray_backslashes(json: &str) -> String {
    let chars: Vec<char> = json.chars().collect();
    let mut out = String::with_capacity(json.len() + 16);
    let mut in_string = false;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if !in_string {
            if c == '"' {
                in_string = true;
            }
            out.push(c);
            i += 1;
            continue;
        }
        match c {
            '"' => {
                in_string = false;
                out.push(c);
                i += 1;
            }
            '\\' => {
                let next = chars.get(i + 1).copied();
                let after = chars.get(i + 2).copied();
                let valid = match next {
                    Some('"' | '\\' | '/') => true,
                    Some('u') => chars.get(i + 2..i + 6).is_some_and(|h| h.iter().all(|c| c.is_ascii_hexdigit())),
                    Some('b' | 'f' | 'n' | 'r' | 't') => !after.is_some_and(|a| a.is_ascii_alphabetic()),
                    _ => false,
                };
                if valid {
                    out.push(c);
                    out.push(next.unwrap());
                    i += 2;
                } else {
                    out.push_str("\\\\");
                    i += 1;
                }
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

#[derive(Default)]
pub(crate) struct RunningReviews {
    blind: std::sync::Mutex<BTreeMap<String, Arc<Cancellation>>>,
    #[cfg(test)]
    pub(crate) blind_provider: std::sync::Mutex<Option<crate::delegate::blind::Fixture>>,
    pruned_decks: Vec<String>,
    failures: std::sync::Mutex<std::collections::BTreeSet<(String, Kind)>>,
    jobs: std::sync::Mutex<BTreeMap<(String, Kind), Arc<Job>>>,
    stopping: std::sync::atomic::AtomicBool,
}

#[derive(Clone)]
pub(crate) enum Baseline {
    Automatic(crate::auto_review::DeckFiles),
    Human(crate::human_edits::DeckBaseline),
}

pub(crate) struct Job {
    pub cancel: Cancellation,
    pub gate: Arc<tokio::sync::Mutex<()>>,
    pub baseline: Option<Baseline>,
    pub settle_now: tokio::sync::Notify,
    captured: bool,
    model: std::sync::Mutex<String>,
    selection: AgentSelection,
    details: std::sync::Mutex<Option<fuzukue_proto::RunDetails>>,
    started_at: std::sync::Mutex<Option<i64>>,
    committed: std::sync::atomic::AtomicBool,
}

impl Job {
    fn details(&self) -> fuzukue_proto::RunDetails {
        self.details.lock().unwrap().clone().unwrap_or_else(|| fuzukue_proto::RunDetails {
            selection: self.selection.clone(), escalated: false, tokens: None, cost_usd: None, duration_ms: None,
        })
    }

    pub(crate) fn mark_committed(&self) {
        self.committed
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

impl Workspace {
    pub(crate) fn review_settings(&self) -> ReviewSettings {
        self.review_settings.lock().unwrap().clone()
    }

    pub(crate) async fn set_review_settings(&self, settings: ReviewSettings) -> Result<()> {
        let previous = self.review_settings();
        for kind in [Kind::Style, Kind::Math] {
            let selection = kind.selection(&settings);
            if selection != kind.selection(&previous) {
                crate::agents::validate(self, &selection).await?;
            }
        }
        let mut current = self.review_settings.lock().unwrap();
        crate::state::save_review_settings(&self.state_dir, &settings)?;
        *current = settings.clone();
        self.hub
            .publish_snapshot(ServerEvent::ReviewSettingsChanged { settings });
        Ok(())
    }

    pub(crate) fn running_review_count(&self) -> usize {
        self.reviews.jobs.lock().unwrap().values()
            .filter(|job| job.started_at.lock().unwrap().is_some()).count()
    }

    pub(crate) async fn request_review(self: &Arc<Self>, deck: String, kind: &str, origin: Option<&str>) -> Result<String> {
        let work = self.admit_work()?;
        let kind = Kind::parse(kind)?;
        ensure!(kind != Kind::Consistency, "整合の確認は、人の編集を閉じた時に自動で始まる");
        ensure!(kind != Kind::Output, "出力査読には節の cards が必要");
        let caller = origin.map(|_| fuzukue_proto::AgentCaller::FrontDesk);
        self.check_review_policy(kind, caller).await?;
        ensure!(
            Path::new(&deck).is_relative(),
            "deck はプロジェクト相対のパスで指定する"
        );
        let project = self.project_dir.canonicalize()?;
        let root = safe_join(&project, &deck)?;
        ensure!(root.is_dir(), "{deck} はディレクトリではない");
        let relative = root.strip_prefix(&project)?;
        let deck = relative.to_string_lossy().replace(std::path::MAIN_SEPARATOR, "/");
        let slug = root
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("project");
        let session = format!("review-{}-{slug}", kind.name());
        let settings = self.review_settings();
        let Some(job) = self.queue_review(&deck, kind, None, &settings) else {
            ensure!(
                !self.reviews.jobs.lock().unwrap().get(&(deck.clone(), kind)).is_some_and(|job| job.captured),
                "{deck} の {} 査読は実行中", kind.name()
            );
            return Ok(session);
        };
        let app = self.clone();
        let origin = origin.unwrap_or(&session).to_owned();
        let review_session = session.clone();
        tokio::spawn(async move {
            let _work = work;
            let _guard = job.gate.lock().await;
            let result = async {
                app.mark_review_running(&deck, kind, &job)?;
                let prepare_project = project.clone();
                let review_root = root.clone();
                let review = tokio::task::spawn_blocking(move || {
                    let (root, _) = resolve_deck(&prepare_project, &review_root)?;
                    Review::prepare(&prepare_project, &root, kind)
                })
                .await??;
                let answer = app
                    .run_reviewer(
                        &review,
                        &app.project_dir,
                        &deck,
                        &deck,
                        &review_session,
                        (&origin, caller),
                        &job,
                    )
                    .await?;
                let _edit = app.comment_edit.lock().await;
                app.with_current_review(&deck, kind, &job, || {
                    let (current, _) = resolve_deck(&app.project_dir, Path::new(&deck))?;
                    ensure!(current == root, "査読中にデッキの場所が変わった");
                    let update = review.append_drafts(&root, &answer)?;
                    let card = update.items.last().map(|item| item.card.clone());
                    app.hub.publish_snapshot(ServerEvent::Comments {
                        deck: deck.clone(),
                        items: update.items,
                    });
                    let count = update.result.context(
                        "査読結果を JSON として読めなかった。返答の末尾を下書きに残した",
                    )?;
                    Ok((count, card.filter(|_| count > 0)))
                })
            }
            .await;
            app.finish_review(&deck, kind, &job, result);
        });
        Ok(session)
    }

    pub(crate) fn publish_review_failure(&self, requested: String, kind: String, error: &anyhow::Error) {
        let deck = resolve_deck(&self.project_dir, Path::new(&requested))
            .map_or_else(|_| if requested == "." { String::new() } else { requested }, |(_, key)| key);
        let mut failures = self.reviews.failures.lock().unwrap();
        if let Ok(kind) = Kind::parse(&kind) {
            failures.insert((deck.clone(), kind));
        }
        self.hub.publish(ServerEvent::ReviewStatus {
            deck, kind, state: "failed".into(), message: format!("{error:#}"),
            model: None, started_at: None, count: None, card: None, details: None,
        });
    }

    pub(crate) fn restore_review_failures(&mut self) {
        let (failures, pruned) = self.hub.restore_review_decks(&self.project_dir);
        self.reviews.pruned_decks = pruned;
        *self.reviews.failures.lock().unwrap() = failures.into_iter()
            .filter_map(|(deck, kind)| Some((deck, Kind::parse(&kind).ok()?)))
            .collect();
    }

    pub(crate) fn pruned_review_decks(&self) -> Vec<String> {
        self.reviews.pruned_decks.iter().filter(|deck| {
            resolve_deck(&self.project_dir, Path::new(deck)).map_or(true, |(_, key)| key != **deck)
        }).cloned().collect()
    }

    pub(crate) fn cancel_review(&self, deck: &str, kind: &str) -> Result<()> {
        let kind = Kind::parse(kind)?;
        let (_, deck) = resolve_deck(&self.project_dir, Path::new(deck))?;
        if let Some(job) = self.reviews.jobs.lock().unwrap().get(&(deck, kind)) {
            job.cancel.cancel();
        }
        Ok(())
    }

    pub(crate) fn expedite_review(&self, deck: &str, kind: Kind) {
        if let Some(job) = self.reviews.jobs.lock().unwrap().get(&(deck.into(), kind)) {
            job.settle_now.notify_one();
        }
    }

    pub(crate) fn stop_reviews(&self) {
        use std::sync::atomic::Ordering;
        let jobs = self.reviews.jobs.lock().unwrap();
        self.reviews.stopping.store(true, Ordering::Relaxed);
        for job in jobs.values() {
            job.cancel.cancel();
        }
        self.stop_blind_reviews();
    }

    fn stop_blind_reviews(&self) {
        for cancel in self.reviews.blind.lock().unwrap().values() {
            cancel.cancel();
        }
    }

    pub(crate) fn stop_reviews_for_swap(&self) {
        self.stop_blind_reviews();
        for job in self.reviews.jobs.lock().unwrap().values() {
            job.cancel.cancel();
        }
    }

    pub(crate) fn active_reviews(&self) -> Vec<ServerEvent> {
        self.reviews
            .jobs
            .lock()
            .unwrap()
            .iter()
            .filter_map(|((deck, kind), job)| {
                job.started_at.lock().unwrap().map(|started| {
                    review_status(
                        deck,
                        *kind,
                        "running",
                        "査読中…".into(),
                        Some(job.model.lock().unwrap().clone()),
                        job.details(),
                        Some(started),
                        None,
                        None,
                    )
                })
            })
            .collect()
    }

    // A successor inherits the lock and unreviewed card baselines. The old CLI turn
    // must finish cancelling before the successor can use that deck and kind.
    pub(crate) fn queue_review(
        &self,
        deck: &str,
        kind: Kind,
        mut baseline: Option<Baseline>,
        settings: &ReviewSettings,
    ) -> Option<Arc<Job>> {
        let mut jobs = self.reviews.jobs.lock().unwrap();
        if self
            .reviews
            .stopping
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            return None;
        }
        let key = (deck.to_owned(), kind);
        let previous = jobs.get(&key);
        let gate = if let Some(previous) = previous {
            if previous.captured {
                return None;
            }
            let baseline = baseline.as_mut()?;
            if previous.cancel.check().is_ok()
                && !previous
                    .committed
                    .load(std::sync::atomic::Ordering::Relaxed)
            {
                match (baseline, previous.baseline.clone()) {
                    (Baseline::Automatic(next), Some(Baseline::Automatic(old))) => next.extend(old),
                    (Baseline::Human(next), Some(Baseline::Human(old))) => next.extend(old),
                    (_, None) => {},
                    _ => unreachable!("baseline type changed for the same reviewer"),
                }
            }
            previous.cancel.cancel();
            if previous.started_at.lock().unwrap().is_some() {
                self.hub.publish(review_status(
                    deck,
                    kind,
                    "superseded",
                    "変更を取り込んだ".into(),
                    Some(previous.model.lock().unwrap().clone()),
                    previous.details(),
                    None,
                    None,
                    None,
                ));
            }
            previous.gate.clone()
        } else {
            Arc::default()
        };
        let job = self.new_review_job(kind, baseline, settings, gate, false);
        jobs.insert(key, job.clone());
        Some(job)
    }

    fn new_review_job(
        &self,
        kind: Kind,
        baseline: Option<Baseline>,
        settings: &ReviewSettings,
        gate: Arc<tokio::sync::Mutex<()>>,
        captured: bool,
    ) -> Arc<Job> {
        let selection = self.agent_selection(None, &kind.agent_name(), kind.selection(settings));
        let model = selection.model.as_deref().map_or_else(
            || format!("{} CLI 既定", selection.backend.label()),
            |model| match selection.backend {
                AgentBackend::Claude => claude_label(model),
                AgentBackend::Codex => model.to_owned(),
            },
        );
        Arc::new(Job {
            cancel: Cancellation::default(),
            gate,
            baseline,
            settle_now: Default::default(),
            captured,
            model: std::sync::Mutex::new(model),
            selection,
            details: Default::default(),
            started_at: Default::default(),
            committed: Default::default(),
        })
    }

    pub(crate) fn with_current_review<T>(
        &self,
        deck: &str,
        kind: Kind,
        job: &Job,
        f: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        let jobs = self.reviews.jobs.lock().unwrap();
        job.cancel.check()?;
        if !jobs
            .get(&(deck.into(), kind))
            .is_some_and(|current| std::ptr::eq(job, current.as_ref()))
        {
            return Err(Cancelled.into());
        }
        f()
    }

    pub(crate) fn mark_review_running(&self, deck: &str, kind: Kind, job: &Job) -> Result<()> {
        self.with_current_review(deck, kind, job, || {
            let started = usage::now();
            *job.started_at.lock().unwrap() = Some(started);
            self.reviews.failures.lock().unwrap().remove(&(deck.into(), kind));
            self.hub.publish(review_status(
                deck,
                kind,
                "running",
                "査読中…".into(),
                Some(job.model.lock().unwrap().clone()),
                        job.details(),
                Some(started),
                None,
                None,
            ));
            Ok(())
        })
    }

    pub(crate) fn finish_review(
        &self,
        deck: &str,
        kind: Kind,
        job: &Job,
        result: Result<(usize, Option<String>)>,
    ) {
        let mut jobs = self.reviews.jobs.lock().unwrap();
        let key = (deck.into(), kind);
        if !jobs
            .get(&key)
            .is_some_and(|current| std::ptr::eq(job, current.as_ref()))
        {
            return;
        }
        jobs.remove(&key);
        let started = *job.started_at.lock().unwrap();
        let mut failures = self.reviews.failures.lock().unwrap();
        let was_failed = failures.remove(&key);
        let (state, message, count, card) = match result {
            Ok((0, _)) if started.is_none() && !was_failed => return,
            Ok((count, card)) => ("done", format!("指摘 {count} 件"), Some(count), card),
            Err(error) if error.is::<Cancelled>() => ("cancelled", "止めた".into(), None, None),
            Err(error) if error.is::<ReviewBlocked>() => ("skipped", error.to_string(), None, None),
            Err(error) => ("failed", format!("{error:#}"), None, None),
        };
        if state == "failed" {
            failures.insert(key);
        }
        self.hub.publish(review_status(
            deck,
            kind,
            state,
            message,
            Some(job.model.lock().unwrap().clone()),
                        job.details(),
            started,
            count,
            card,
        ));
    }

    async fn check_review_policy(&self, kind: Kind, caller: Option<fuzukue_proto::AgentCaller>) -> Result<()> {
        let name = kind.agent_name();
        let policy = self.effective_agent_policy(&name).await?;
        ensure!(policy.enabled, "作業役 {name} は無効になっている");
        if let Some(caller) = caller {
            ensure!(policy.allows(caller), "{}から作業役 {name} を呼ぶことは許可されていない", caller.label());
        }
        Ok(())
    }

    pub(crate) async fn run_reviewer(
        &self,
        review: &Review,
        project: &Path,
        deck: &str,
        path: &str,
        session: &str,
        origin: (&str, Option<fuzukue_proto::AgentCaller>),
        job: &Job,
    ) -> Result<String> {
        let cancel = &job.cancel;
        self.check_review_policy(review.kind, origin.1).await?;
        cancel.check()?;
        let backend = job.selection.backend;
        if self.usage.blocked(backend.usage_key()).is_some() {
            return Err(ReviewBlocked(backend.usage_key()).into());
        }
        let statuses = self.lean_status.lock().unwrap().clone();
        let mut prompt = review.prompt(path, &statuses)?;
        if review.kind == Kind::Lean {
            let mut check = crate::lean_reading::doc_check(project).await?;
            let links = crate::lean_reading::card_links(project);
            for finding in &mut check.findings {
                if finding.card.is_none() {
                    finding.card = links.get(&finding.declaration).cloned();
                }
            }
            prompt.push_str(&harness_format!(
                "\nServer documentation findings. Verify these review points and use the corresponding card IDs when reporting them:\n{}\n",
                "\nサーバーが検出した doc コメントの指摘候補。内容を確かめ、報告できるものには対応するカード ID を添える:\n{}\n",
                serde_json::to_string_pretty(&check)?,
            ));
        }
        let settings = job.selection.chat_settings();
        let invocation = crate::agent_usage::Invocation::new(
            self, origin.0, origin.1, &review.kind.agent_name(), &crate::mail::session_key(),
            &format!("{}の査読", review.kind.name()), Some(job.selection.clone()),
        );
        let timer = std::time::Instant::now();
        let mut metadata = crate::agent_usage::RunMetadata::default();
        let history = self.history.turn(project, format!("reviewer:{}", review.kind.name()), origin.0, None).await;
        let lease = crate::mcp::workspace_read::ReviewLease::open(self, session)?;
        let launch = lease.as_ref().map(|lease| &lease.launch);
        let result = match backend {
            AgentBackend::Claude => {
                crate::claude::run_workspace_review(self, project, &settings, &prompt, cancel, |model| {
                    let _ = self.with_current_review(deck, review.kind, job, || {
                        let started = *job.started_at.lock().unwrap();
                        let mut current = job.model.lock().unwrap();
                        let model = claude_label(model);
                        if *current != model {
                            self.hub.publish(review_status(
                                deck,
                                review.kind,
                                "running",
                                "査読中…".into(),
                                Some(model.clone()),
                                fuzukue_proto::RunDetails {
                                    selection: AgentSelection { model: Some(model.clone()), ..job.selection.clone() },
                                    ..job.details()
                                },
                                started,
                                None,
                                None,
                            ));
                            *current = model;
                        }
                        Ok(())
                    });
                }, &mut metadata, launch)
                .await
            }
            AgentBackend::Codex => async {
                let codex = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return Err(Cancelled.into()),
                    result = self.codex() => result?,
                };
                codex
                    .run_workspace_review(project, session, &prompt, settings, cancel, &mut metadata, answer_schema(), launch)
                    .await
            }.await,
        };
        if let Some(history) = history {
            history.finish(result.as_ref().ok().cloned(), Vec::new()).await;
        }
        let result = cancel.check().and(result);
        *job.details.lock().unwrap() = Some(fuzukue_proto::RunDetails {
            selection: AgentSelection {
                model: metadata.model.clone().or_else(|| job.selection.model.clone()),
                effort: metadata.effort.clone().or_else(|| job.selection.effort.clone()),
                backend,
            },
            escalated: false, tokens: metadata.tokens, cost_usd: metadata.cost_usd, duration_ms: Some(timer.elapsed().as_millis() as u64),
        });
        invocation.finish(self, &result, metadata);
        if result.as_ref().is_err_and(|error| !error.is::<Cancelled>()) && self.usage.blocked(backend.usage_key()).is_some() {
            return Err(ReviewBlocked(backend.usage_key()).into());
        }
        result
    }
}

fn claude_label(model: &str) -> String {
    match model {
        "claude-sonnet-5-5" => "Claude Sonnet 5.5".into(),
        "claude-opus-5-5" => "Claude Opus 5.5".into(),
        "claude-haiku-5-5" => "Claude Haiku 5.5".into(),
        "claude-fable-5-2" => "Claude Fable 5.2".into(),
        _ => model.into(),
    }
}

#[derive(Debug)]
struct ReviewBlocked(&'static str);

impl std::fmt::Display for ReviewBlocked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} の使用量上限で今回は見送った", self.0)
    }
}

impl std::error::Error for ReviewBlocked {}

fn review_status(
    deck: &str,
    kind: Kind,
    state: &str,
    message: String,
    model: Option<String>,
    details: fuzukue_proto::RunDetails,
    started_at: Option<i64>,
    count: Option<usize>,
    card: Option<String>,
) -> ServerEvent {
    ServerEvent::ReviewStatus {
        deck: deck.into(),
        kind: kind.name().into(),
        state: state.into(),
        message,
        model,
        details: Some(details),
        started_at,
        count,
        card,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reviewer_prompts_keep_the_shared_voice_and_review_instructions() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let rules = crate::rules::resolve(dir.path())?;
        for kind in Kind::ALL {
            let instructions = kind.instructions(dir.path(), &rules)?;
            let review = Review {
                kind, instructions: instructions.clone(), cards: Vec::new(),
                scoped: false, passages: Vec::new(), captured: None, custom: None,
            };
            let prompt = review.prompt(".", &[])?;
            assert!(prompt.contains(&instructions), "{}", kind.name());
            assert!(prompt.contains(crate::rules::conversation_rules()), "{}", kind.name());
        }
        Ok(())
    }

    #[test]
    fn reads_findings_with_unescaped_tex() {
        // 実際に Codex が返した形: TeX のコマンドがエスケープされていない。
        let answer = r#"{"findings":[{"card":"c","text":"$\alpha_{H_n}(r)\le 2\frac{1}{2}$ と \nabla f、\theta、改行\nあり、\"引用\""}]}"#;
        let found = parse_findings(answer).unwrap();
        assert_eq!(found[0].text, "$\\alpha_{H_n}(r)\\le 2\\frac{1}{2}$ と \\nabla f、\\theta、改行\nあり、\"引用\"");
        assert!(parse_findings(r#"{"findings":[{"card":"c","text":"あ"}]}"#).unwrap()[0].text == "あ");
    }

    #[test]
    fn collapses_double_escaped_tex() {
        let answer = r#"{"findings":[{"card":"c","text":"$\\\\exp(-2t^2)$ と行末 \\\\ 1"}]}"#;
        assert_eq!(parse_findings(answer).unwrap()[0].text, "$\\exp(-2t^2)$ と行末 \\\\ 1");
    }

    #[test]
    fn schema_requires_card_and_text() {
        let schema = answer_schema();
        assert_eq!(schema["properties"]["findings"]["items"]["required"], serde_json::json!(["card", "text"]));
    }
}
