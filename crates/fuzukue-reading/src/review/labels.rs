//! Text shared by the Markdown exporter and generated-file recognition.

/// Language of the generated labels.
#[derive(Clone, Copy)]
pub enum Language {
    En,
    Ja,
}

impl Language {
    /// Parse a language code accepted by the Reader and the standalone commands.
    pub fn parse(code: &str) -> anyhow::Result<Self> {
        match code {
            "en" => Ok(Self::En),
            "ja" => Ok(Self::Ja),
            _ => anyhow::bail!("language must be en or ja"),
        }
    }
    pub(crate) fn labels(&self) -> &'static Labels {
        match self {
            Self::En => &EN,
            Self::Ja => &JA,
        }
    }

    /// Return the ISO language code of the generated page.
    pub fn code(&self) -> &'static str {
        match self {
            Self::En => "en",
            Self::Ja => "ja",
        }
    }

    pub(crate) fn findings_count(
        &self,
        total: usize,
        [fatal, major, minor, form]: [usize; 4],
    ) -> String {
        match self {
            Self::En => format!(
                "{total} findings: fatal {fatal}, major {major}, minor {minor}, form {form}."
            ),
            Self::Ja => format!(
                "指摘は全 {total} 件：致命的 {fatal}、要修正 {major}、軽微 {minor}、表現 {form}。"
            ),
        }
    }

    pub(crate) fn step_findings(&self, count: usize) -> String {
        match self {
            Self::En => format!("Findings on this step ({count})"),
            Self::Ja => format!("このステップへの査読の指摘（{count} 件）"),
        }
    }
}

// Share field names with the optional standalone template labels.
macro_rules! labels {
    ($($field:ident),+; $($group:ident: [$($key:ident),+]),+ $(,)?) => {
        pub(crate) struct Labels {
            $(pub(crate) $field: &'static str,)+
            $(pub(crate) $group: [&'static str; [$(stringify!($key)),+].len()],)+
            pub(crate) rules: [&'static str; 7],
        }

        impl Labels {
            pub(crate) fn template_label(&self, name: &str) -> Option<&'static str> {
                match name {
                    $(stringify!($field) => Some(self.$field),)+
                    _ => {
                        $(
                            let keys = [$(stringify!($key)),+];
                            if let Some(index) = keys.iter().position(|&key| key == name) {
                                return Some(self.$group[index]);
                            }
                        )+
                        None
                    }
                }
            }
        }
    };
}

labels! {
    toc, display, btn_all, btn_none, btn_key, caution, local,
    h_summary, h_intro, h_spine, h_compare, h_demo, h_review, h_prior, h_indicator, h_glossary,
    spine_lead, h_diagram, diagram_hint, legend_key, legend_semi, legend_standard,
    compare_lead, th_class, th_content, th_role, th_origin, th_risk, th_evidence,
    meta_loc, meta_origin, meta_deps, none, why_tier, l2, l3,
    review_conclusion, review_reading, review_reading_text,
    h_panel, th_view, th_who, th_scope, th_result, h_mychecks, h_findings, findings_hint,
    f_target, f_from, f_ruling, f_all, f_evidence,
    th_prior, th_op, th_hyp, th_overlap, th_state, prior_hole, prior_uncited, prior_admitted,
    verdict, axes_h, rules_h, changers_h, indicator_note, generated;
    tiers: [tier_key, tier_semi, tier_standard],
    origins: [origin_inherited, origin_standard, origin_adapted, origin_new],
    severities: [sev_fatal, sev_major, sev_minor, sev_form],
    statuses: [st_confirmed, st_suspected, st_unchecked],
    levels: [lvl_unknown, lvl_weak, lvl_mid, lvl_strong],
    axis_names: [ax_correctness, ax_novelty, ax_delta, ax_strength, ax_significance, ax_literature],
    verdicts: [v_none, v_needs_work, v_short_or_preprint, v_journal],
}

const EN: Labels = Labels {
    toc: "Contents",
    display: "Display",
    btn_all: "Open all",
    btn_none: "Close all",
    btn_key: "Open key steps only",
    caution: "This is a reading aid for work under review. 'No gap found' means none was found within the stated scope; it is not a proof of correctness. Novelty is judged only against the sources actually read. Nothing here is a formal proof or a journal's opinion.",
    local: "A single local file; it is not published anywhere.",
    h_summary: "Summary",
    h_intro: "The result",
    h_spine: "Proof spine",
    h_compare: "Key versus non-key steps",
    h_demo: "Try it",
    h_review: "Review",
    h_prior: "Relation to prior work",
    h_indicator: "Viability indicator",
    h_glossary: "Glossary",
    spine_lead: "Each step shows one line (always visible), the key reasoning (folded) and the line-by-line proof (folded again). Key steps are open to the key reasoning.",
    h_diagram: "Dependency diagram",
    diagram_hint: "An arrow means 'uses'. Orange is key, purple semi-key, grey standard. Click a box to jump to the step.",
    legend_key: "key",
    legend_semi: "semi-key",
    legend_standard: "standard",
    compare_lead: "Key = necessary for the theorem and not in the literature as such (by the work's own claim). Semi-key = necessary but a combination of known methods. Standard = in the literature as such.",
    th_class: "Class / id",
    th_content: "Content",
    th_role: "Role",
    th_origin: "Origin",
    th_risk: "Remaining risk",
    th_evidence: "Evidence of checking",
    meta_loc: "Source",
    meta_origin: "Origin",
    meta_deps: "Uses",
    none: "none",
    why_tier: "Why this class",
    l2: "Key reasoning",
    l3: "Line-by-line proof",
    review_conclusion: "Conclusion of the review",
    review_reading: "How to read this",
    review_reading_text: "'No gap found' means none was found within the scope in the table. An adversarial reader can only report holes it finds; it cannot show that none exist. Reviews by language models are not an expert's check or a formal proof.",
    h_panel: "Panel and scope",
    th_view: "Viewpoint",
    th_who: "Who",
    th_scope: "Scope checked",
    th_result: "Result",
    h_mychecks: "Checks I ran myself",
    h_findings: "Findings, most serious first",
    findings_hint: "'Confirmed' means a counterexample or a reproducible calculation exists; 'suspected' means reasoning only.",
    f_target: "Target",
    f_from: "Raised by",
    f_ruling: "Ruling",
    f_all: "whole work",
    f_evidence: "Evidence",
    th_prior: "Prior work",
    th_op: "Operator placement",
    th_hyp: "Assumptions and conclusions",
    th_overlap: "Overlap with the key steps",
    th_state: "What was checked",
    prior_hole: "Largest unchecked gap",
    prior_uncited: "Candidates raised by reviewers (outside the source list, bibliographic details unchecked)",
    prior_admitted: "What the work itself already concedes",
    verdict: "Verdict",
    axes_h: "Axes",
    rules_h: "Rules for the verdict",
    changers_h: "What would change this verdict",
    indicator_note: "This is an aid to judgment, not a prediction of acceptance and not a certificate of correctness. Read the reason for each axis.",
    generated: "Generated",
    tiers: ["key", "semi-key", "standard"],
    origins: ["inherited from prior work", "standard", "known method, adapted", "new (as claimed)"],
    severities: ["fatal", "major", "minor", "form"],
    statuses: ["confirmed", "suspected", "unchecked"],
    levels: ["unknown", "weak", "mid", "strong"],
    axis_names: ["Evidence of correctness", "Novelty of the key part", "Difference from the closest prior work", "Strength of the theorem", "Significance and applications", "Completeness of the literature check"],
    verdicts: ["no verdict", "needs work before submission", "short paper or preprint", "journal candidate"],
    rules: [
        "Without a completed review, no verdict is issued and correctness is 'unknown'.",
        "One confirmed fatal finding caps the verdict at 'needs work'.",
        "Correctness cannot exceed 'mid' without an expert's check or a formal proof.",
        "If the closest prior work was not read, novelty cannot exceed 'mid'.",
        "'Short paper or preprint' needs correctness at least 'mid' and a concretely stated difference from the closest prior work.",
        "'Journal candidate' needs novelty 'strong', theorem strength at least 'mid', and that the prior-art attack did not break the claim.",
        "With a limited source list, the literature axis cannot exceed 'mid'.",
    ],
};

const JA: Labels = Labels {
    toc: "目次",
    display: "表示",
    btn_all: "すべて開く",
    btn_none: "すべて閉じる",
    btn_key: "キーだけ開く（既定）",
    caution: "これは査読中の研究の読解資料です。「穴が見つからなかった」は、記した範囲で見つからなかったという意味で、正しさの証明ではありません。新規性は、実際に読んだ資料の範囲でしか判断していません。形式証明でも、雑誌の判断でもありません。",
    local: "手元で開く単一ファイルです。ネット上には公開されていません。",
    h_summary: "結論の要約",
    h_intro: "何を示した研究か",
    h_spine: "証明の骨格",
    h_compare: "キーと標準の比較",
    h_demo: "手を動かす",
    h_review: "査読",
    h_prior: "先行研究との違い",
    h_indicator: "論文になりそうかの指標",
    h_glossary: "用語メモ",
    spine_lead: "各ステップは「1 行説明（常に表示）→ 要点（畳み）→ 全行の証明（さらに畳み）」の 3 段です。キーは要点まで開いてあります。",
    h_diagram: "依存関係図",
    diagram_hint: "矢印は「使う」。オレンジ＝キー、紫＝準キー、灰＝標準。クリックでそのステップへ飛びます。",
    legend_key: "キー",
    legend_semi: "準キー",
    legend_standard: "標準",
    compare_lead: "キー＝定理に必要で、かつ先行研究にそのままは無い（その研究自身の主張による）。準キー＝必要だが既知の手法の組み合わせ。標準＝文献にそのまま載る。",
    th_class: "分類・id",
    th_content: "内容",
    th_role: "役割",
    th_origin: "由来",
    th_risk: "残るリスク",
    th_evidence: "確認の証拠",
    meta_loc: "原稿",
    meta_origin: "由来",
    meta_deps: "使うもの",
    none: "なし",
    why_tier: "分類の理由",
    l2: "要点",
    l3: "全行の証明",
    review_conclusion: "査読の結論",
    review_reading: "この結果の読み方",
    review_reading_text: "「穴が見つからなかった」は、表の範囲で見つからなかったという意味です。敵対的な視点が出せるのは見つけた穴だけで、穴が無いことの証明にはなりません。言語モデルによる査読は、専門家の点検でも形式証明でもありません。",
    h_panel: "査読の体制と範囲",
    th_view: "視点",
    th_who: "担当",
    th_scope: "検査の範囲",
    th_result: "結果",
    h_mychecks: "私が自分で行った確認",
    h_findings: "指摘の一覧（重大な順）",
    findings_hint: "「確定」は反例か再現できる計算があるもの、「疑い」は推論のみのものです。",
    f_target: "対象",
    f_from: "出した視点",
    f_ruling: "私の裁定",
    f_all: "全体",
    f_evidence: "根拠",
    th_prior: "先行研究",
    th_op: "方程式・対象の置き方",
    th_hyp: "仮定と結論",
    th_overlap: "キーとの重なり",
    th_state: "確認の状態",
    prior_hole: "未確認の最大の穴",
    prior_uncited: "査読者が挙げた候補（出典の範囲外・書誌は未確認）",
    prior_admitted: "その研究自身が既に認めていること",
    verdict: "判定",
    axes_h: "軸",
    rules_h: "判定の規則",
    changers_h: "この判定を変える項目",
    indicator_note: "これは判断の補助で、採否の予測でも正しさの認証でもありません。軸ごとの根拠を読んで判断してください。",
    generated: "生成日",
    tiers: ["キー", "準キー", "標準"],
    origins: ["先行研究から継承", "標準", "既知の手法の改変", "新規（その研究の主張）"],
    severities: ["致命的", "要修正", "軽微", "表現"],
    statuses: ["確定", "疑い", "未確認"],
    levels: ["未確認", "弱", "中", "強"],
    axis_names: ["正しさの証拠", "キー部分の新規性", "最も近い先行研究との差分", "定理の強さ", "意義・応用", "文献調査の完全性"],
    verdicts: ["判定なし", "投稿前に要作業", "短報または arXiv まで", "ジャーナル候補"],
    rules: [
        "査読が完了していなければ判定を出さず、正しさは「未確認」とする。",
        "確定した致命的な指摘が 1 つでもあれば、判定は「投稿前に要作業」までとする。",
        "専門家の点検か形式証明がなければ、正しさは「中」を超えない。",
        "最も近い先行研究を読んでいなければ、新規性は「中」を超えない。",
        "「短報または arXiv まで」には、正しさが「中」以上で、最も近い先行研究との差分が具体的に書けることを要する。",
        "「ジャーナル候補」には、新規性が「強」、定理の強さが「中」以上、先行研究への攻撃が破れなかったことを要する。",
        "出典を限定した調査では、文献調査の軸は「中」を超えない。",
    ],
};
